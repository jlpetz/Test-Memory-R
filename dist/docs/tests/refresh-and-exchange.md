# Refresh, page exchange and coherence storm

Most of TMR's tests write a pattern and read it back within milliseconds, each thread in its own
memory. Refresh leaves its data alone before it checks it; the page exchange passes memory
between threads, so data one core wrote is checked by another; the coherence storm has every core
write to the same few cache lines at once.

| Test | What it checks | Plans | A step |
|---|---|---|---|
| Mem-RefreshAuto | data that sits still keeps its value | quick, stability, widths, all | 15 s |
| Mem-Refresh128/256/512 | the same, at a fixed width | widths, all | 15 s |
| Mem-Refresh-FlushAuto | the same, each check sure to read DRAM | all | 15 s |
| Mem-PageExchange | memory copied from thread to thread | quick, stability, all | 30 s |
| Mem-PageExchange-Rewrite | memory checked and rewritten by other threads | quick, stability, all | 30 s |
| Mem-PageExchange-Invert | memory checked and inverted by other threads | all | 30 s |
| Mem-CoherenceStorm | cache lines that every core writes | all | 30 s |

None of them is in `plan=perf`. Run any of them by name: `tmr test=Mem-CoherenceStorm`, or
`tmr test=Mem-PageExchange*` for the three exchange tests, `tmr test=Mem-Refresh*` for the five
Refresh tests. A step stops at the end of the pass it's in once its time is up, so it runs a
little past it.

## Mem-Refresh: data left alone

### What it's for

DRAM stores each bit as a charge in a tiny capacitor, and the charge leaks. The memory refreshes
every row in turn to top it up (every 32 ms on DDR5, 64 ms on DDR4). If the refresh timings are
too loose or too tight, the modules run hot, or a cell is weak, a bit can fade before its row is
refreshed. Most tests read their data back too soon to see that. Refresh writes its data and
then waits.

### How it works

Each pass has three phases:

1. **Write.** Each thread writes a fixed pattern over its test area.
2. **Wait.** The thread sleeps for `wait_ms`, 500 ms by default: about 15 of DDR5's refresh
   windows, 8 of DDR4's. Nothing touches the data but the memory's own refresh.
3. **Check.** It reads every word back and compares it with the pattern.

The test area is twice the CPU's caches (every core's L1 and L2, plus one L3), from the start of
each thread's memory. By the time a word is read back, the rest of the pass has pushed it out of
the caches, so the check reads DRAM. A step repeats passes for 15 s: about two dozen.

### Variants

- **Mem-RefreshAuto** runs at the widest SIMD width the CPU has: 512-bit on a CPU with AVX-512
  (AMD Zen 4 and later, some Intel), else 256-bit. `width=256` (or `128`, `512`) picks one for
  every Auto test in the run. The step is named for the width it ran, with `_A` after it:
  `Mem-Refresh512_A`.
- **Mem-Refresh128, Mem-Refresh256, Mem-Refresh512** each keep their width, for `plan=widths` to
  compare. The width matters little here, as the wait takes most of the time. A CPU without
  AVX-512 leaves Mem-Refresh512 out of the run.
- **Mem-Refresh-FlushAuto** flushes the area out of the caches once it's written, before the
  wait, so the check is sure to read DRAM. The plain tests' eviction holds on most PCs but isn't
  guaranteed (a virtual machine's shared cache, say), and a cached copy would hide a fade. The
  flush costs speed (about a fifth, measured on an earlier version with a shorter wait). It runs
  at the Auto width, shown as `Mem-Refresh-Flush512_A` and the like.

### Which plans run it

`plan=stability` (the default) and `plan=quick` run Mem-RefreshAuto, tenth of their 13 tests.
`plan=widths` runs Mem-Refresh128/256/512/Auto side by side. Mem-Refresh-FlushAuto runs
only under `plan=all` or by name.

### Reading its output

While it runs, the status line shows the phase: `Writing`, then the wait's countdown,
`Waiting, 0.4 s of 0.5 s left (wait_ms)`, then `Verifying`.

The step's speed (MiB/s) is far below your memory's bandwidth. That's on purpose: the wait counts
as the test's time, because holding the data is the test. Compare it only with another Refresh
step at the same wait, never with a bandwidth figure. The Cycles column counts passes.

A bad word gets an error line in the log, with its address, the value expected, the value found
and the bits that differ. The step's header counts it as a test error; the final summary counts
it under the test, and its bits go into the Failing Bits table. Errors here that the other tests
don't show point at retention: the refresh timings, or the modules' temperature.

### Settings

These are set in a TMR `.json` config's test entry, not on the command line (the examples in
`configs\` show a whole config):

- **`wait_ms`**: the wait, in milliseconds. 500 by default. The step's `parameter=` shows it when
  set, such as `Wait(30min)`. The wait stops at Ctrl+C within a tenth of a second; the pass it
  stopped goes unchecked.
- **`flush_before_verify`**: `true` flushes each chunk before the wait, as Mem-Refresh-FlushAuto
  does. In a config, set it on the entry.
- **`extent`** and **`chunk`**: a config's entry tests the config's default extent unless it
  gives its own. The built-in tests' twice-the-caches is `{"mode": "cache_total", "fraction": 2.0}`.

**Bit fade.** A long wait finds slow leaks that half a second can't: minutes of it. This entry
writes all of each thread's memory, waits 30 minutes, then checks it, once:

```json
{"enabled": true, "function": "Mem-RefreshAuto", "cycles": 1,
 "extent": {"mode": "full_allocation"}, "chunk": {"mode": "whole"}, "wait_ms": 1800000}
```

Its countdown reads `Waiting, 00:29:12 of 00:30:00 left (wait_ms)`.

### Caveats

- The built-in tests cover only the start of each thread's memory: twice the caches, tens of MB a
  thread, not all of it. The seal (below) covers the rest, under load rather than idle. For all of
  it idle, use the bit fade entry above.
- A TM5 `.cfg`'s RefreshStable imports as Mem-RefreshAuto over all of the memory, flushing, with
  the 500 ms wait (TM5's has no wait setting). As a `.cfg`'s test 0, it is the seal instead.

## The page exchange: Mem-PageExchange, -Invert and -Rewrite

### What it's for

In a real program, data one core writes is read by another, through the fabric that links the
cores, the dies (CCDs on Ryzen) and the sockets. These three tests do that on purpose, modelled on
Google's stressapptest (GSAT), to catch faults in the memory controller under mixed traffic from
many cores, in the links between dies or sockets, or in data handed from core to core.

On a Ryzen 5 8600G with two known faults, the exchange caught both: a read fault in bit 31, only
through its non-temporal (NT) stores (Mem-SimpleV2-Auto caught that one far more often), and a
fault in a stored pair of bits that SimpleV2 missed in 231 steps.

### How it works

Each thread's memory, all of it, is cut into tiles of 2 MiB (a large page). Then every thread
takes tiles from every thread's memory, one at a time, in an order shuffled each pass: each pass
reaches every tile once, and a tile goes to whichever thread draws it. A tile is held by one
thread at a time, and a thread that finds one busy skips it. Every tile records which pattern it
holds, so any thread can check it.

A tile's first visit in a step writes it a pattern. After that, each visit checks the tile
against its pattern and then works it. The three tests differ in that work:

- **Mem-PageExchange** copies. Each thread starts with one of its own tiles in hand. A visit
  copies the tile it took into the one in hand, checking each word on the way, hands that one
  back, and keeps the tile it took for the next copy. So patterns move around memory as well as
  between threads.
- **Mem-PageExchange-Rewrite** checks the tile, then writes a new pattern over it.
- **Mem-PageExchange-Invert** checks each word and writes back its inverse in place, in one pass.

The patterns are TMR's seven native families (ident, random, switch, lines, linepair, quiet and
header), each written as itself and as its complement in turn. The copy and the rewrite store
non-temporally (straight to DRAM, so the next thread reads the tile from DRAM); the invert stores
through the caches.

When the step's time is up, every thread stops taking tiles, then checks its own tiles one last
time.

The tests have no width variants. They run at 512 bits on a CPU with AVX-512 (or under
`width=512`), else at 256; `width=128` runs them at 256.

### Which plans run them

`plan=stability` and `plan=quick` run Mem-PageExchange fourth and Mem-PageExchange-Rewrite ninth,
apart, so a fault that comes and goes meets one or the other; together they add about a minute to
each cycle. Mem-PageExchange-Invert runs only under `plan=all` or by name. Each step is 30 s.

### Reading their output

The step report is laid out like any other test's. Two things differ:

- **A thread's errors are those it found**, not necessarily in its own memory. Each visit that
  finds bad words logs them as any test does, then adds a line saying whose tile it was:
  ```
  Error found by Mem-PageExchange in ...: those 2 words are tile 17 of thread 3 (node 0),
  holding random-~P (thread 5's image), last written by thread 5 (node 0), and checked by thread 1 (node 0)
  ```
  So you can see whether errors follow one thread's memory, one writer, or one node. A visit that
  finds bad words writes the pattern whole again, so each is counted once.
- **The speed counts what the test reads and writes**: a copy counts the tile read and the tile
  written. On a Ryzen 5 8600G, the NT copy ran 44% faster than a cached one, and faster than
  Mem-SimpleV2-Auto; on a Ryzen 7 5700X, 23% faster than a cached one.

The Cycles column counts passes over the tiles, each thread doing its share of each. On a tile's
first visit in a sealed run, its seal is checked: what that finds goes in the step's seal count,
and the line says the tile held the seal.

### Settings

In a TMR `.json` config's entry:

- **`exchange_share`**: whose tiles a thread takes. `any` (the default) is every thread's, so
  tiles cross dies and sockets as they come. `node` is the threads' on its own NUMA node only: the
  exchange without the links between sockets. `cross` is the other nodes' only, so every visit
  crosses a link; on a PC with one node, it is `any`. On a 2-socket EPYC, `node` ran 1.5 to 2.1
  times as fast as `any`, `cross` about half.
- **`exchange_stores`**: `nt` or `cached`. `nt` by default for the copy and the rewrite, `cached`
  for the invert.
- **`exchange_invert`**: Mem-PageExchange only. The share of its threads, 0 to 100%, that invert
  while the rest copy, as GSAT's copy and invert threads work together. At 50, every other thread
  inverts. 0 by default.
- **`chunk`** sets the tile size: `{"mode": "absolute", "size": "2MiB"}` is the default.
- **`program`** picks the pattern families, such as `["random", "switch"]`.

Set ones show in the step's `parameter=`, such as `Share(cross),Stores(nt)`.

### Caveats

- The tests run native patterns only. A config's numbered `pattern_mode` is refused, and
  `pattern-mode=` on the command line leaves them their own patterns.
- Under `errors=halt`, the whole step halts, every thread, since the memory is shared. The bad
  words stay as found.
- If the tile size doesn't divide a thread's memory, its last tile is shorter. It is still tested,
  but a copy can't copy it into a full tile, so it rewrites it in place.

## Mem-CoherenceStorm: cache lines every core writes

### What it's for

When cores write to the same cache line, the line has to move from core to core, and every core
has to agree on what it holds. That's the coherence fabric's job (Infinity Fabric on Ryzen, so
FCLK). This test loads that fabric and almost nothing else, modelled on GSAT's cache coherency
test. An error is a write lost or changed between cores.

### How it works

A few shared structures (2 by default) each hold one byte for every thread, 64 threads to a cache
line. With 64 threads or fewer, every thread shares each line; with more, a line's 64 are spread
over the thread list, so they sit on different dies. Each thread adds 1 to its own byte in a
structure picked at random, 1000 times, then checks that each of its bytes holds its own count.
No thread writes another's byte, but they share lines, so each write pulls its line from
whichever core wrote it last. A wrong byte counts as one error. A cycle is 256 checks.

The test barely touches DRAM and writes none of the memory TMR allocated. Its threads all start
together, so the lines are contested from the first write.

### Which plans run it

Only `plan=all`, or by name: `tmr test=Mem-CoherenceStorm`. A step is 30 s.

### Reading its output

- The line that starts the step gives its extent as 4 KiB: the memory it is given and leaves
  alone.
- **Data and Speed count increments**, one byte each, not memory traffic. Each increment waits for
  its line to arrive: about 27 ns a thread on a Ryzen 5 8600G, 55 ns on a Ryzen 7 5700X.
- An error's log line names the thread, the structure, the line and byte, the value expected and
  found, the bits that differ, and how many threads (on which nodes) share that line. These errors
  don't go into the Failing Bits table.

### Settings

In a TMR `.json` config's entry:

- **`storm_lines`**: the shared structures, 1 to 4096 (default 2). Fewer means each line is
  written more often.
- **`storm_increments`**: increments between checks, 1 to 16777216 (default 1000).

Set, they show in `parameter=` as `Lines(2),Bumps(1000)`.

### Caveats

- On a CPU with one die (a Ryzen APU such as the 8600G, or a single-CCD Ryzen such as the 5700X)
  the lines never leave the die, so the test can't reach the links between dies. It does most on
  multi-die and multi-socket machines.
- It takes no seal and checks no memory, so a pass here says nothing about the modules
  themselves.

## The seal, and these tests

In a sealed run (the default, `seal=tmr`), TMR writes all of memory with a seal pattern at each
cycle's start, and each test checks the seal before it uses a part, and reseals it after. What
that check finds is the step's seal errors: the `seal` count in its header, the Seal column of the
final summary. That is data that sat untouched under load, as Refresh's sat untouched idle. More
on [the seal](seal-and-bench.md).

- **Refresh** checks the seal over its whole area before its first pass and reseals it when the
  step ends.
- **The page exchange** checks each tile's seal on its first visit, and reseals every tile at the
  step's end.
- **Mem-CoherenceStorm** takes no seal.

## The results file

Each step is an entry in its cycle's `tests`, named as the step was (`Mem-Refresh512_A`). The
fields to read: `errors`, `seal_errors`, `throughput_mib_s`, `cycles_completed_min` and
`cycles_completed_max` (the fewest and most passes a thread made), `stopped_by` (`time` when it
ran its time), and `threads`, with each thread's errors.

Back to [TMR's tests](README.md).
