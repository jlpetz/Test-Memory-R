# Test Harnesses and Execution Tiers

**Status**: reference. Reflects the code as of 2026-09-14 (branch `clippy-cleanup-todo20`).
**Related**: TODO #19 (the tier decision), TODO #28 (Tier 3, unbuilt),
`doc/simd_codegen_rules.md` (why macros, not generics), `doc/extent_chunk_modes.md`
(what extent/chunk mean), `doc/test_parameters.md` (`parameter_context` fields).

A "tier" in TMR is **how much of its own loop a test owns**. It is not a measure of test
quality, importance, or SIMD width — every tier gets full SIMD. This document explains why
there is more than one harness, what each one gives you and takes away, which tests use which,
and how to pick a tier for a new test.

---

## 1. Vocabulary

These four words get used loosely in conversation; in this doc they mean exactly this:

| Term | Meaning | Example |
|---|---|---|
| **Registration name** | The string a config or `tests=` filter matches. Several names can map to one function. | `Mem-StuckBit-Flush256` |
| **Test function** | The `unsafe fn(blocks, thread_id, error_mode, timing, config, progress) -> TestStats` that the thread pool calls once per thread. Owns everything from there down. | `stuck_bit_test_256_multi` |
| **Harness / scaffolding** | Shared code the test function delegates *orchestration* to. Two exist: `run_phased_test` (owns the loop) and `TestRunner` (owns bookkeeping only). | `test_harness.rs`, `test_scaffolding.rs` |
| **Kernel** | The innermost loop that actually issues the loads and stores. Not a distinct code object in TMR — it is a *region* inside the test function, expanded there by macro. | the body of `stuck_bit_write_verify!` |

The last row is the important one and the reason the tiers look the way they do. See §2.

---

## 2. Why there is more than one harness

Two independent constraints, one from the compiler and one from the tests themselves.

### 2.1 The compiler constraint: a kernel cannot be a callable object

`#[target_feature(enable = "avx512f")]` does **not** propagate across a function-call, trait-method,
or closure boundary. If the kernel lives behind any such boundary, its body is compiled at the
crate baseline (`x86-64-v3`) instead of the width you asked for, silently. This is exactly what
killed `test_framework.rs` (a generic `TestPattern` trait + `run_interleaved_test()`, deleted
2026-04-13): correct results, no error, SIMD quietly downgraded.

So shared orchestration has only three legal forms:

1. **Generic + `#[inline(always)]`, closures monomorphized at the call site** — the closure body
   gets inlined *into* the `#[target_feature]` caller, so the width survives. This is
   `run_phased_test`. It works, but only because every type parameter is concrete and the harness
   is `#[inline(always)]`; weaken either and the SIMD silently drops.
2. **Macro expansion at the call site** — the kernel text lands inside the `#[target_feature]` fn
   with no boundary at all. This is `stuck_bit_impl!`, `refresh_impl!`, `mirror_move_v2_impl!`,
   `spd_read_impl!`, and the width variants generally.
3. **Ordinary functions that only run between chunks** — where no SIMD is in flight, a normal call
   is free. This is `TestRunner`: every method is an `#[inline]` field read/write called *between*
   chunks, never inside a kernel.

Form 3 is why `TestRunner` can be a plain struct with methods and still cost nothing. Form 1 is
why `run_phased_test` takes closures rather than a trait. Form 2 is why nearly every hot loop in
`tests.rs`, `bandwidth_tests.rs`, and `latency_tests_v2.rs` is inside a `macro_rules!`.

### 2.2 The test constraint: not every test fits one loop nest

`run_phased_test` hard-codes this nest:

```
cycle
└─ block
   └─ chunk (linear element range: chunk_start..chunk_end)
      └─ write_read_cycles
         ├─ test_fn   × test_reps
         ├─ fence
         ├─ [flush_range_to_dram]        (if flush_before_verify)
         └─ verify_fn × verify_reps
```

That covers "fill a linear range, optionally shake it, read it back" — which is most of TM5's
shape. It cannot express:

- more than one pattern constant per chunk visit (StuckBit needs three phases with alternating
  constants),
- a **delay** between write and verify (Refresh sleeps 64 ms per chunk),
- a **non-linear** access order within the chunk (Random, Stride, CacheBust),
- a loop level **outside** the chunk walk (Stride loops over strides, then chunks),
- **two ranges** with different roles (BlockMove uses first half as source, second as destination).

Tests in that list own their loop and take bookkeeping from `TestRunner` instead. That is the
entire distinction between Tier 1 and Tier 2. It is about *loop shape*, nothing else.

---

## 3. The tiers at a glance

| Tier | Mechanism | Who owns the loop | Gives you | Costs you |
|---|---|---|---|---|
| **1 — Phased** | `run_phased_test` (`test_harness.rs:113`) | Harness | Cycle/chunk/block walk, shutdown checks, error-mode dispatch, byte + op accounting, progress, `TestStats`, `skip_init` for dependent tests, `flush_before_verify`, TM5 `write_read_cycles`/`test_reps`/`verify_reps` | The fixed nest above. One pattern per phase, linear chunk range, no delays |
| **2 — Loop-owning** | `TestRunner` (`test_scaffolding.rs:33`) | **Test** | Extent sizing, block prep, timers, cycle counter, shutdown probe, throttled progress, error-mode dispatch, `TestStats` construction | You write the cycle/block/chunk walk and the byte accounting yourself |
| **— Unscaffolded** | none (hand-rolled in a macro) | Test | nothing | You duplicate all of the above |
| **3 — Concurrent** | worker/mailbox model — **not built** (TODO #28) | Coordinator routes *jobs*; workers own access | cross-thread coordination | does not exist yet |

Tiers 1 and 2 are not independent implementations: `run_phased_test` **builds on** `TestRunner`
internally (`test_harness.rs:154`), so extent sizing, the info log, timers, progress, and stats
have one source of truth across both. Tier 1 adds the loop and the phased-specific accounting
(`total_operations`, `ops_per_wrc` byte math, `block_metas`, `check_mask`, `skip_init`).

---

## 3a. The seal: every correctness test gets it (TODO 74)

**What it is for.** Every test checks its own data almost as soon as it writes it: microseconds to
milliseconds later. The seal checks data that has sat untouched for about one whole step (seconds
to minutes) while the step hammered the rest of memory: retention under load, which is sensitive
to refresh timings, heat and neighbouring-row disturbance. It also catches a test that wrote
outside its own chunk. TM5 does the same with its test 0 and numbers these errors 0; nothing else
in TMR waits under load (Mem-Refresh waits 64 ms, idle).

**How a run uses it** (`seal.rs`, `seal=tmr|tm5|off`, `seal-width=`):

```
cycle start   seal all memory                               "Sealing memory"
each step, each chunk of a correctness test:
              check the seal         → its errors are the step's Seal column (TM5 #0)
              the test's chunk body  (its own fill moves in here; a test whose first op writes needs none)
              reseal                 → the chunk sits sealed until the next step reaches it
cycle end     check all memory                              "Final seal check"
```

- A **mirror** test's data *is* the seal (TM5's `Capable_UseTst0ForGenAndCheck`): it swaps the
  sealed words and puts them back, and the seal check after it is its verify, so those errors are
  the mirror's own (TM5 numbers them by the mirror step). It reseals only a chunk that failed.
- **Bandwidth, latency, Bench and Mem-Random** never take it (`runner::seal_use`). The worker checks
  the part of their extent still sealed before they run (a separate dispatch, so their times and
  MiB/s don't include it) and reseals what they left before the next step that takes the seal. A
  dependent step (`skip_init`, e.g. a Bench-Verify) must therefore directly follow its writer: a
  sealed step in between would reseal the data, and the plan refuses it.
- A TM5 import's test 0 (RefreshStable) is the seal: enabled, the run is sealed and a `0` in
  `Test Sequence` is a `Seal-Check` step; disabled, the run is unsealed.
- A step that runs more than one cycle of its own (a duration) wraps its first cycle only and
  reseals its extent when it finishes, so it pays for the seal once per step; a step cut short
  reseals its extent too. A run in which no step takes the seal isn't sealed at all.

**What a test has to do.** Tier 1 gets it for free: `run_phased_test` wraps each chunk when
`config.seal.wrap` is set. A Tier-2 test calls `runner.check_seal(start, len)` before it works a
chunk and `runner.reseal(start, len)` after, in bytes from the extent's start (both are no-ops
in an unsealed run); a test that fills a range up front must fill it per chunk instead when
`runner.seal_wrap()` (Mem-BlockMove's source half). A new `Mem-*` test takes the seal unless
`runner::seal_use` lists it, and `every_sealed_test_honours_the_seal` fails until it honours it.
Opting out per test: `"seal": false` in its JSON entry.

**Cost.** One read pass and one non-temporal write pass per chunk per step. On the dev box, at
28 GiB, a seal pass runs at ~90 GiB/s (NT, any width) and a check at ~60 GiB/s at 256 bits (`auto`;
the 512-bit check ran 15-20% slower): about 0.8 s per step. On 1usmus_v3 that is 3-7% on a
sequential SimpleTest step, nothing on a mirror step (its data is the seal anyway), and the
strided SimpleTest steps got 5-20% faster, since they no longer write a strided fill up front.

---

## 4. Tier 1 — the phased harness (`run_phased_test`)

### What a Tier-1 test actually is

A Tier-1 test function is a **dispatcher plus three closures**. It picks a pattern mode, then
makes exactly one `run_phased_test` call per mode with the closures for that mode. Because each
call site must monomorphize separately, a test with 7 pattern modes has 7 `run_phased_test` calls
— that is why `bench_init_multi` and `bench_verify_multi` each contain 7, and
`simple_test_v2_sequential` contains 6. This duplication is deliberate and load-bearing: merging
them behind a dynamic mode value would put a branch in the hot loop or a boundary around the
kernel.

### The closure contract

```rust
Init:   FnMut(&ChunkCtx)          // once per block at startup, unless skip_init
Test:   FnMut(&ChunkCtx)          // per chunk per cycle — the "shake"; may be |_| {}
Verify: FnMut(&ChunkCtx) -> u64   // per chunk per cycle — returns error count
```

`ChunkCtx` (`test_harness.rs:25`) carries `ptr`, `chunk_start`, `chunk_end`, `cycle`, `thread_id`,
`check_mask`. Note it gives the closure a **block base pointer plus an index range**, not a slice
— the kernel does its own `ptr.add(i)`.

`init_fn` is required even when `skip_init` is true. It is not called in that case, but it
documents the pattern the verify closure expects, which is what a dependent test needs for
repair (see TODO #13).

### Which tests, and why they fit

| Registration names | Function | Why Tier 1 fits |
|---|---|---|
| `Mem-SimpleV2`, `-128/-256/-512/-Auto` | `simple_test_v2_*` | Literally the TM5 shape the harness was modelled on: fill a linear range, read it back `verify_reps` times, repeat `write_read_cycles` times. Strided mode (`simple_test_v2_strided`) still walks a linear chunk — the stride is inside the kernel, not a loop level above it |
| `Mem-SimpleNT-128/-256/-512/-Auto` | `simple_test_nt_*` | Same shape; only the store primitive changes (NT stores, 4× manual unroll per `doc/nt_stores.md`) |
| `Mem-MirrorV2`, `-128/-256/-512/-Auto` | `mirror_move_v2_*` | The mirror swap is a perfect `test_fn`: it mutates the chunk in place and leaves it verifiable. Uses `test_reps` to do N round-trips before checking. Pattern-agnostic, so it also runs dependently |
| `Bench-Init-{TM5,TMR}-{0,1,2}`, `Bench-Init-TMR-3` | `bench_init_multi` | Pure fill throughput — `test_fn` = fill, `verify_fn` = no-op |
| `Bench-Verify-{TM5,TMR}-{0,1,2}`, `Bench-Verify-TMR-3` | `bench_verify_multi` | Pure verify throughput. Uses `skip_init` to run dependently on a prior `Bench-Init` |

**Gap**: none of the `Bench-*` names have `-128/-256/-512` width variants. Only
`Bench-{Init,Verify}-{TM5,TMR}-N` exist. Tracked as a known gap in CLAUDE.md.

---

## 5. Tier 2 — the loop-owning scaffolding (`TestRunner`)

### What TestRunner is and is not

It is **a bag of bookkeeping, not an orchestrator**. Its module doc says so explicitly, and the
reason is §2.1: an orchestrator would have to call back into the kernel, and that call boundary
would strip the SIMD. So the test keeps its loop and calls the runner between chunks.

### The method set and when to call each

| Method | Call it | Purpose |
|---|---|---|
| `TestRunner::new` | once, at the top | Extent sizing, the extent (`test_memory::extent`: the first bytes of the thread's span), the chunk (resolved once from the extent), the per-thread info log with the extent's page sizes and chunks, starts timers. **Returns `(runner, extent)`** — keep `extent` as a local so you can hold it while calling `&mut self` methods |
| `restart_clock()` | after untimed setup | Starts the clock again, so a fill or a chain build counts toward neither the duration nor the throughput. The bandwidth and latency tests call it. The ticker shows their setup as "Filling memory" / "Building pointer chain" until then |
| `check_seal(start, len)` / `reseal(start, len)` | before / after working a chunk | The seal around the chunk (§3a), in bytes from the extent's start. No-ops unless the step wraps (`seal_wrap()`). The check's errors are the seal's, not the test's, but halt it under `errors=halt` |
| `chunks(piece_size)` | once per piece | The piece's `ChunkSpread` (TODO 76): `count()` chunks of exactly `chunk()` bytes (the test's chunk, or the piece when it is shorter), at `start(k)`, spread evenly from 0 to the piece's end. Chunks overlap by under one chunk in total when the chunk doesn't divide the piece, so every word must depend only on its position. Byte offsets: convert to your own units. `half_chunks` is the same for a copy from a piece's first half to its second; `chunk_bytes()` is the bare size, for tests whose chunks aren't ranges (Mem-Random) |
| `begin_cycle()` | top of each cycle | Increments and returns the cycle number |
| `add_bytes(n)` | per chunk, before the halt check | Byte accounting, so a halted or interrupted test reports what ran. **You compute the multiplier** — StuckBit passes `chunk() * 6` (3 writes + 3 reads), Refresh `chunk() * 2`. Overlaps count each time |
| `should_halt(cycle_errors)` | after a verify | Error-mode dispatch: panics on `Panic`, returns `true` on `Halt`, `false` on `Log` |
| `shutdown_requested()` | between chunks | Relaxed load of `SHUTDOWN_REQUESTED`. **Never inside a kernel** |
| `update_progress()` / `update_progress_in_cycle()` | end of cycle / between chunks | Throttled to 250 ms; no-op without a progress sink. Tier 1 calls the in-cycle one per chunk, since one piece can be a whole cycle |
| `should_continue()` | end of cycle | The timing/cycle gate |
| `commit_cycle_errors(n)` | end of cycle | Folds the cycle's errors into the running total |
| `finish_aborted(cycle_errors, ops)` / `finish_completed(ops)` | on exit | Builds `TestStats`. Aborted = Halt-on-error or Ctrl+C; completed = timing gate reached |

The standard Tier-2 skeleton:

```rust
let (mut runner, test_blocks) = TestRunner::new(/* … */);
// hoist every config field you need into a local HERE, never read one in a loop
let flush_before_verify = config.flush_before_verify;

loop {
    runner.begin_cycle();
    let mut cycle_errors = 0u64;

    let base = extent.ptr as *mut $simd_type;
    let spread = runner.chunks(extent.test_size);
    let chunk = spread.chunk() / lanes;

    for k in 0..spread.count() {
        let start = spread.start(k) / lanes;
        runner.check_seal(spread.start(k), spread.chunk());
        // …your loop nest over start..start + chunk, whatever shape it needs…
        //    kernel goes here, macro-expanded, no fn boundary
        runner.reseal(spread.start(k), spread.chunk());

        runner.add_bytes(spread.chunk() * /* your own multiplier */);
        if runner.should_halt(cycle_errors) { return runner.finish_aborted(/*…*/); }
        if runner.shutdown_requested()      { return runner.finish_aborted(/*…*/); }
    }

    runner.commit_cycle_errors(cycle_errors);
    runner.update_progress();
    if !runner.should_continue() { return runner.finish_completed(/*…*/); }
}
```

### Which tests, and the specific reason each one needs Tier 2

This is the table to read if you are deciding where a new test goes.

| Registration names | Function / macro | Why the phased harness cannot express it |
|---|---|---|
| `Mem-StuckBit{,128,256,512,Auto}`, `Mem-StuckBit-Flush{,Auto,128,256,512}` | `stuck_bit_impl!` (widths), `stuck_bit_test_multi` (scalar) | **Three phases per chunk visit with alternating constants**: write P1→verify, write P2→verify, write P1→verify. `run_phased_test` has one `test_fn`/`verify_fn` pair and one pattern per chunk visit. The third phase (P1 again) specifically catches transition-induced flips, so it is not reducible to two runs of a two-phase test |
| `Mem-Refresh{,128,256,512,Auto}`, `Mem-Refresh-Flush{,128,256,512,Auto}` | `refresh_impl!` (widths), `refresh_stable_multi` (scalar) | **A 64 ms `sleep` between write and verify, per chunk.** The harness has no delay phase, and the sleep has to be ordered after the fence and after the optional flush — otherwise a cached copy masks the bit-fade the test exists to find (the #26 bug) |
| `Mem-CacheBust` | `cache_busting_multi` | **Non-linear access order.** Walks the chunk at `CACHE_BUSTING_STRIDE`, and the whole write/verify shape switches on the `stride_patterns` parameter. `ChunkCtx` hands the closure a linear `chunk_start..chunk_end`; the offset-then-stride walk is a loop nest the harness does not have |
| `Mem-Random` | `random_torture_multi` | **RNG-driven access order** over the piece, indexed by multiply-high (`rng * len >> 64`, any length), iterating `rng_sequences`. Mismatches OR into an accumulator; a nonzero one replays the chunk from its saved RNG state to count and log (TODO 76). Also inits once up front and never re-inits, which does not map onto per-cycle `test_fn`/`verify_fn`. (Reads only today — random *writes* are an open item, TODO #19 step 6) |
| `Mem-Stride` | `stride_access_multi` | **A loop level above the chunk walk.** The nest is cycle → block → *stride* → chunk → subdivision. The harness's nest is cycle → block → chunk, and the stride loop cannot be pushed inside a chunk without changing what is measured |
| `Mem-BlockMove` | `block_move_multi` | **Two ranges with different roles** — first half source, second half destination — plus a `copy_directions` switch. `ChunkCtx` describes one range |

Note the pattern: every Tier-2 justification is a *loop-shape* or *timing* need, never a SIMD or
performance need. If a proposed test's reason for wanting Tier 2 is "it'll be faster," that is not
a reason — Tier 1 kernels are inlined into the `#[target_feature]` fn and are equally fast.

**Also note**: `Mem-CacheBust`, `Mem-Random`, `Mem-Stride`, and `Mem-BlockMove` have **no width
variants** — one scalar registration each. At the `x86-64-v3` baseline LLVM may autovectorize
them to 256-bit, but the width is not chosen or guaranteed.

---

## 6. The unscaffolded measurement tests

`bandwidth_tests.rs` and both latency modules don't use `run_phased_test`. Since TODO 76 they do
use `TestRunner`, like Tier 2: it sizes their extent, runs their timer, cycle gate, progress and
shutdown, and builds their `TestStats` (the latency tests wrap it in `LatencyTestStats`). They
register chunk mode `whole`, one chunk, the extent. Their setup runs before `restart_clock()`:
the bandwidth tests' page-faulting fill, and the latency tests' chains, built in place on the
extent (Sattolo's algorithm for the cycle, inside-out Fisher-Yates for the data targets; no heap).

What sets them apart:

- **No error checking** → `should_halt` and the error-mode dispatch, roughly half of what
  `TestRunner` does, is moot. `spd_*` takes `_error_mode` and always reports `error_count: 0`.
- **Bandwidth deliberately does not chunk.** `spd_*` walks the whole extent per cycle (one
  chunk under `whole`) and checks shutdown once per cycle. Chunking would insert a loop boundary
  into the thing being measured; shutdown responsiveness is bounded by one extent pass instead.
- **Latency needs a different return type.** `TestFunction::Latency` returns `LatencyTestStats`
  (percentiles, TSC samples), with `TestRunner::finish_completed` as its `basic_stats`. One cycle
  is one sample.

| Registration names | Module | Return type |
|---|---|---|
| `Spd-{L1,L2,L3}-{Read,Write,Copy}-Auto` | `bandwidth_tests.rs` | `TestStats` |
| `Spd-{DRAMSmall,DRAMFull}-{Read,Write,Copy}-Auto` | `bandwidth_tests.rs` (NT stores for Write/Copy) | `TestStats` |
| `Lat-{L1,L2,L3,DRAM,DRAMFull}-{Read,Write,Copy}` (15) | `latency_tests.rs` (v1) | `LatencyTestStats` |
| `Lat-V2-*`, `Lat-V2P-*`, `Lat-NTW-*` | `latency_tests_v2.rs` | `LatencyTestStats` |

---

## 7. Calibration — outside the pipeline entirely

`calibration.rs` is not a tier. It does not go through the thread pool, does not return
`TestStats`, and is not in `get_test_function_by_name`. It runs single-threaded via its own
`ProbeEngine` (pointer-chasing latency sweep), reached from `main.rs` (`--calibrate-extended`).

It has two allocation modes: `run()` / `run_extended()` allocate their own buffer, while
`run_with_buffer()` / `run_extended_with_buffer()` borrow an existing `MemoryBuffer` so
calibration uses the same page types as the test plan with no extra alloc. Whether it should
*always* share the main allocator is an open design question in CLAUDE.md.

---

## 8. Tier 3 — concurrent / worker model (not built)

TODO #28. A coordinator routes **jobs** (region descriptors: test id, address range, stride,
pattern, duration); workers own and execute all memory access locally. The settled rule is that
individual loads/stores are **never** routed through a queue — any handoff between test logic and
the actual access destroys the signal being measured. Handoff granularity is per work unit
(~32 MB, ~100 ms–1 s of kernel time), never per access.

The two tests that need it: **cross-thread page exchange** (thread A writes a page and enqueues
it, thread B verifies A's pattern and writes its own) and coherence-storm variants. Both need
thread-to-thread coordination that no per-thread harness can provide, because a Tier-1/2 test
function is handed its own blocks and never talks to another thread.

---

## 9. Picking a tier for a new test

Work down this list; take the first match.

1. **Does it need two threads to coordinate mid-test?** → Tier 3. It does not exist, so this
   blocks on TODO #28.
2. **Is it a pure throughput or latency measurement with no correctness check?** → follow
   `bandwidth_tests.rs` / `latency_tests_v2.rs`. Consider adopting `TestRunner` for the
   boilerplate even so; there is no reason not to beyond it not having been done.
3. **Can it be written as: fill a linear range → optionally mutate it → read it back, with one
   pattern per pass?** → **Tier 1.** Add one `run_phased_test` call per pattern mode. This is the
   default; prefer it.
4. **Otherwise** → **Tier 2** with `TestRunner`. Expect to justify it with a loop-shape reason
   from §5's table: multiple phases per chunk, a delay, a non-linear order, a loop level above the
   chunk walk, or two ranges.

Whichever tier: the kernel goes in a `macro_rules!` expanded inside the `#[target_feature]` fn.
Never a generic fn, trait method, or closure that is not monomorphized at the call site. Then
**read the emitted assembly** (`cargo rustc --release -- --emit asm`) and confirm the width and
instructions you asked for are actually there — see `doc/simd_codegen_rules.md`. Both codegen
traps this project has hit were silently lossy: correct results, wrong test.

### Known applications of this rule

TODO #19's remaining test backlog is already assigned:

- **Address-in-address** — Tier 2 (or Tier 1; the pattern is address-derivable, so it may fit)
- **Random writes** — Tier 2, extends `Mem-Random`
- **Moving inversions** — Tier 2. Per-element read→modify→write, which no current test does. Write
  it as one concrete test; do **not** build a `march_element!` macro until a second march-shaped
  test appears
- **Cross-thread page exchange** — Tier 3, blocked
- **Extended bit fade** — Tier 2, a config variant of Refresh with 1–60 min delays

---

## 10. How a test function gets called (the layer above all tiers)

For completeness, since "which tier" only makes sense relative to this boundary:

`runner.rs` resolves a registration name to a `TestFunction` (`get_test_function_by_name`, plus
dynamic construction for the auto-dispatch and `Lat-V2*` families), then calls
`ThreadPool::execute_test`. Each worker thread waits on a `Barrier`, then calls the test function
**once** with the full slice of **its own** blocks:

```
f(blocks_slice, thread_id, error_mode, &timing, &config, Some(&progress))
```

So the harness boundary is *inside* the test function, per thread. The pool never sees a cycle, a
chunk, or a kernel — it sees one call that returns `TestStats`. Interleaving across a thread's
blocks is the test's (or its harness's) job, which is why both `run_phased_test` and every Tier-2
loop have a `for block in test_blocks` level: it exists to share one timer across blocks rather
than run N sequential timed tests.

`TestFunction` has four variants; only two are live:

- `MultiBlock(fn) -> TestStats` — everything in §4, §5, §6-bandwidth
- `Latency(fn) -> LatencyTestStats` — everything in §6-latency
- `Simple`, `WithConfig` — **dead.** The variants and their dispatch arms exist
  (`runner.rs:4037-4038`) but nothing constructs them; `thread_pool.rs:303` has
  `unreachable!("Only MultiBlock and Latency tests are registered")`. These are the pre-MultiBlock
  single-pointer signature, left behind by the migration

---

## Appendix: observations found while writing this doc

Items 1, 2 and 5 are **tracked as TODO #69** (A, B and C respectively) — not fixed here. Items 3
and 4 were corrections to TODO #19 itself and were applied on 2026-09-18.

1. **`TestFunction::Simple` / `WithConfig` are dead code**, along with
   `run_test_with_memory_stages` in `runner.rs`, whose only reachable arms return `Err`. Deleting
   them makes `thread_pool.rs`'s match exhaustive, replacing a runtime `unreachable!` with a
   compile-time guarantee. → **#69 A**
2. **`ThreadPool::execute_latency_test` and `WorkItem::RunLatencyTest` are dead** — no caller
   outside their own definitions; the live path is `execute_test` → `WorkItem::RunTest`, which
   handles `Latency` itself. The dead path also **hardcodes `read_latency_multi` regardless of
   `test_name`**, so if it were ever revived, `Lat-*-Write` and `Lat-*-Copy` would silently run
   the read test. → **#69 B**
3. ~~TODO #19's tier table omits three harness families~~ — **fixed 2026-09-18**: #19 now scopes
   its table to the correctness tests and cross-references this doc for the rest.
4. ~~TODO #19 step 2's `TestRunner::new` line numbers are stale~~ — **fixed 2026-09-18**: the
   numbers (tests.rs:1516/2542/3371/3517/3658/3765) had drifted by ~250-950 lines in three months
   and are now replaced by function names. Same applies to this doc: **prefer the function or macro
   name; treat any line number here as a hint that may have drifted.**
5. **`block_move_multi` reads `config.parameter_context` inside the chunk loop**
   (`tests.rs:2863`) — an `Option` chain plus `.expect()` per chunk. Between chunks, so not a hot
   path violation, but every other test hoists this to a local before the loop. → **#69 C**

**Also noted, and deliberately not filed as a defect:** the scaffolding duplication in
`bandwidth_tests.rs` and the two latency modules (§6). It is partly principled and partly debt, so
it is recorded as **#69 D** — a decision to make, including the option of declining it.
