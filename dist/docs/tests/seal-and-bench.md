# The seal and the Bench tests

This page covers two things that sit beside the [stability tests](stability.md). **The seal** is
not a test you pick. It runs around the stability tests in every run that has them, and its
errors have a column of their own. **The Bench tests** time how fast TMR writes and checks each
of its patterns. They are diagnostics: you run them by name, or with `plan=all`.

## The seal

### What it catches

Every test checks its data soon after it writes it, microseconds to milliseconds later. The seal
checks data that has sat untouched for about a whole step, seconds to minutes, while that step
worked the rest of memory hard. That finds faults that need time under load. Data can fade before
it is refreshed, which refresh timings and heat affect. Heavy use of nearby rows can also disturb
it. The seal also catches a write that lands at the wrong address, outside the chunk a test was
working on.

TM5 does this with its test 0, and numbers those errors 0. The seal is TMR's version of test 0.
Nothing else in TMR leaves data waiting under load. Mem-Refresh waits too, but the memory is
idle while it does.

One example, from a Ryzen 5 8600G with a known fault (2026-11-03): two bits of one stored line
went wrong together. The seal caught them 13 times. Mem-SimpleV2's own checks caught them in none
of 231 steps.

### How a run uses it

```
cycle start   seal all memory               🔒 Sealing memory (tmr-256): ...
each step     for each chunk of a correctness test:
                check the seal              -> the step's seal errors
                the test works the chunk
                reseal the chunk            -> it stays sealed until the next step reaches it
cycle end     check all memory              🔒 Final seal check (tmr-256): 0 errors, ...
```

The seal is written with non-temporal stores. These go straight to the memory, instead of waiting
in the CPU's cache.

### Which steps take it

As a run starts, it prints its plan as a table, and the table has a Seal column:

| Seal | Tests | What happens |
|---|---|---|
| ✓ | Mem-* tests, except the ones below | the seal is checked before each chunk and resealed after it |
| data | Mem-MirrorV2-* | the test works on the sealed data itself. The seal check after it is the test's own check, so those errors are the test's |
| – | Mem-Random, Mem-CoherenceStorm, and every Bench, Lat and Spd test | never sealed. TMR checks the sealed memory such a step will overwrite before it starts, outside its time. Before the next sealed step, it reseals it |

What that check finds goes in the unsealed step's seal count. That is a fault in data an earlier
step left; the step itself didn't find it. A JSON config can leave one test unsealed with
`"seal": false` in its entry.

### Which runs are sealed

- `seal=tmr` (the default), `seal=tm5` or `seal=off`, on the command line or in a config
  (`system.seal`).
- `plan=stability`, `quick`, `widths` and `all` are sealed. A run where no step takes the seal is
  unsealed: `plan=perf`, or `test=Bench-*`. Its plan says
  `🔓 Seal: off (no step in this plan takes it)`, and its error counts read `(0 test + 0 WHEA)`.
- In a TM5 config, the seal is test 0 (RefreshStable). If test 0 is enabled, the run is sealed
  with TMR's pattern. A `0` in its Test Sequence becomes a `Seal-Check` step, which checks every
  chunk and reseals any that failed; its errors are its own. If test 0 is disabled, the run is
  unsealed.

### Its two patterns, and the cycles

- **`tmr`** (the default). Every 16 bytes hold a value and its inverse. The value is a hash of
  the address and the cycle. No two words anywhere hold the same value, so a write that lands at
  the wrong address always shows.
- **`tm5`** is TM5's own test 0 pattern, bit for bit. Each 4 KiB page gets a 48-byte motif made
  from the page number, repeated across the page. Different pages can share a motif (page 1 and
  page 65,536 do), so a write that lands at the wrong address can go unseen. Use it to compare
  with TM5.

Both put each 8-byte word next to its inverse. On DDR4's 64-bit bus, every data line flips on
every other transfer. On DDR5's 32-bit transfers about half flip, as with random data.

Cycles go in pairs. The second cycle of a pair writes every bit inverted, and the next pair takes
a new hash (`tm5` only inverts). So every sealed bit holds 1 for one cycle and 0 for the other.
Memory still holding the last cycle's seal fails the check. With one cycle (`plan=quick`, a `test=`
run or `cycles=1`), each bit is held only one way. Run an even number of cycles to hold both.

### Reading its results

Here is where it shows on the console. The window shows the same figures, some in its own words.

- **The plan:** `🔒 Seal (tmr-256, calibrated): all memory is sealed ...` names the pattern and
  the width, and where the width came from: `calibrated`, `rule`, or the setting that set it.
- **The setup:** `Seal calibration: Loaded (256 bits, <date>)`, or
  `Stale (hardware changed, re-run --calibrate-seal)`, or `None (run --calibrate-seal to calibrate)`.
- **Each step's report:** the headline reads `N errors (T test + S seal + W WHEA)`. The seal part
  is what the checks before this step's chunks found. `Failing bits: test (bits 12, 31), seal (bit 5)`
  names the bits that failed. The `Intermediate:` line gives the time and speed of the seal's fill
  and check for this step, which aren't in its `Test:` time.
- **Each cycle's end:** `🔒 Final seal check (tmr-256): 2 errors (bit 5), 1.2s, 24.00 GiB @ 20827 MiB/s`.
- **The final summary:** Total Errors is split into Test, `Seal 2 (1 in the final checks)` and
  WHEA. The per-test table and Errors by Step each have a Seal column. Intermediate Operations
  totals the seal's fills and checks.
- **The results file:**
  - for each step, `cycles[].tests[].seal_errors` and `seal_failing_bits`;
  - for each cycle, `cycles[].seal`, with `final_check_errors` and the seal's times;
  - for the run, `overall_stats.seal_errors`;
  - in `run_config.execution`, `seal` (`tmr-256`) and `seal_width_source`.

Seal errors are errors. They count in the total and fail the run (exit code 1), and
`errors=halt` stops at one. A cycle that stops early, on Ctrl+C or a halt, skips its final check,
and the results file marks it `partial`.

### Its width, and --calibrate-seal

The seal's code runs at 128, 256 or 512 bits. Only its speed depends on the width: every width
writes and checks the same bits.

- `seal-width=auto` (the default) takes this machine's seal calibration. With none, it takes 256.
- `seal-width=128|256|512` sets a width. If `seal-width=` isn't given, `width=` sets the seal's
  width too. TMR refuses a width the CPU lacks before anything runs.
- `calibrate=off` ignores the calibration, and takes 256.

`tmr --calibrate-seal` measures it, in a few seconds. One thread fills and checks a buffer 4 times
the size of the L3 cache (512 MiB at least), at each width the CPU has. It does one warm-up round,
then 5 timed rounds. It picks the fastest width by its median time. If a narrower width is as
fast, it picks that one, because a narrower width draws less power. "As fast" means within the
fastest width's own run-to-run spread, and never more than 1% slower than it.
`📊 Seal Width Calibration` shows each width's fill and check speed and its fill + check time
(median [min..max]), then the width it picked.

- **Saving:** it asks before replacing a calibration it already has (`[Y/n]`). `--yes` saves
  without asking, and `--no-save` keeps the old one.
- **Other ways to run it:** `--calibrate-all` runs the cache calibration and then this one. The
  window's Calibrate the caches and the seal does both.
- **Where it's kept:** the result goes in `tmr-cfg.json`, and only the machine that measured it
  uses it. `tmr --show-calibration` shows it.
- **When it runs:** only when you ask. A run never measures it on its own.

Calibrate on stable memory: if the check finds a bad word, the calibration fails.

Why measure it: on one Intel Xeon (2026-10-06, 28 GiB, 4 threads), every width filled at about
90 GiB/s. But the 512-bit check ran at 52-55 GiB/s, against 58-63 GiB/s at 128 and 256 bits. AMD
CPUs measured showed no such penalty.

### What it costs

Each step reads every chunk once and writes it once for the seal, outside the step's `Test:`
time. Each cycle also writes all memory once at its start and reads it all once at its end. On
that Xeon at 28 GiB, this came to about 0.8 s a step. On TM5's 1usmus_v3 config it added 3-7% to
a sequential SimpleTest step. It added nothing to a mirror step, whose data is the seal anyway.
`seal=off` saves this time, but you lose what the seal catches. Use it only to compare.

## The Bench tests

### What they measure

The Bench tests time each pattern on your machine with nothing else in the step. `Bench-Init-*`
writes a pattern, and `Bench-Verify-*` reads it back and checks it. They measure what a pattern
costs to write and check, not whether your memory is stable.

On DRAM, most patterns run at the memory's speed. Their figures differ mostly where a pattern's
arithmetic is the limit. Use them to compare patterns, widths and TMR versions on one machine.

One measurement (2026-10-11, half the memory, physical cores): Ident, Random, Switch, Lines,
Linepair and modes 10, 13 and 30 all verified within about 2% of each other. A Ryzen 5 8600G (DDR5-6000,
6 cores) ran them at 58,200-59,800 MiB/s, and a Ryzen 7 5700X (DDR4-3600, 8 cores) at
45,100-46,500 MiB/s.

### How they work

- **Extent:** each step covers all of each thread's memory, in 4 MiB chunks, and makes 10 passes
  over it.
- **Bench-Init** writes every chunk once a pass, and checks nothing.
- **Bench-Verify** first writes its pattern once, outside its time: that is the pattern fill on
  the `Intermediate:` line. Then it reads and checks every chunk once a pass. A wrong word is a
  real memory error, reported as for any test.
- **The pattern** is the same on every pass, so a Bench-Verify always knows what to expect.
- **Stores:** Bench-Init-Seal-* writes with non-temporal stores, as the seal does. The others use
  ordinary stores, as Mem-SimpleV2 does. An ordinary store makes the CPU read each line before
  it writes it, so these Init figures run below Verify ones on the same memory.
- **No seal:** they never take the seal (`–` in the plan's Seal column).

### The patterns

| Name ends | Pattern |
|---|---|
| `-TM5-0` | mode 0, after TM5's mode 0: a pseudo-random word, every other 4 KiB page inverted |
| `-TM5-1` | mode 1, TM5's linear step, every other 64-byte line inverted |
| `-TM5-2` | mode 2, TM5's evolving lines: each line built from a seed and a step, both changing from line to line, starting again every 4 KiB page |
| `-TMR-10` | mode 10: each word is its index XOR a constant |
| `-TMR-11` | mode 11: the index XOR two parameters |
| `-TMR-13` | mode 13: a hash of each word's index |
| `-TMR-20` | mode 20: mode 2's lines, each hashed from its address |
| `-TMR-30` | mode 30, bus flip: each transfer of a line is the inverse of the one before |
| `-Ident` | every word of the run holds a different value |
| `-Random` | strongly mixed words, so each data line sees every short run of bits |
| `-Switch` | every data line toggles on every transfer |
| `-Lines` | each line an arithmetic chain hashed from the line (TM5 mode 2's idea) |
| `-Linepair` | lines in pairs, the second the inverse of the first (TM5 mode 1's idea) |
| `-Quiet` | Switch, with one data line held steady |
| `-Header` | Switch, with words 0 and 4 of each line unique, so a misplaced half-line shows |
| `-Seal-TMR`, `-128`, `-256`, `-512` | the seal's `tmr` pattern |
| `-Seal-TM5`, `-128`, `-256`, `-512` | the seal's `tm5` pattern |

The numbered ones (TM5-0 to TMR-30) are what Mem-SimpleV2 writes under `pattern-mode=` or a
config's `pattern_mode`, and they run Mem-SimpleV2's own code for it. The named families, Ident to
Header, are the patterns a Mem-SimpleV2 step writes by default, each one alone. Switch, Quiet,
Header and TMR-30 depend on the memory's transfer width. It comes from `beat=`, or else from the
memory type: 64 bits for DDR4, 32 for DDR5.

### Widths

- **The numbered and named pairs** have no width in their names. They run at the run's width:
  `width=`, or else the widest the CPU has (512 bits with AVX-512, else 256). The results file
  records it, in `run_config.tests[].simd_width_bits`. To compare widths, run once with
  `width=256` and once with `width=512`.
- **The Seal pairs** come named for a width (`-128`, `-256`, `-512`), or with none. One with none
  follows `width=`, or else the seal's own `auto` width (above). `seal-width=` doesn't change
  them.
- **Missing instructions:** a run leaves out a 512-bit test on a CPU without AVX-512, and says so:
  `ℹ  Left out N test(s) this CPU can't run`.

### Running them

Of the plans, only `plan=all` runs them. You can also name them, with wildcards and commas:

```
tmr test=Bench-*                                 all 46, 23 Init and Verify pairs, once
tmr test=Bench-Init-Switch,Bench-Verify-Switch   one pair
tmr test=Bench-*-Seal-TMR-*                      the seal's pattern at each width
```

A `test=` run takes one cycle unless you give `cycles=` or `duration=`; `plan=all` takes 3. A run of
Bench tests alone is unsealed.

### How long a step takes

Each step makes 10 passes over all the memory under test, and a Bench-Verify adds one write
outside its time. So a step takes about 10 × memory ÷ speed. For example, with 28 GiB under test
at 40,000 MiB/s, a step takes about 7 s, and all 46 take about 6 minutes.

### Reading their results

The figure is on the step report's `Test:` line: `Test: 7.2s, 280.00 GiB @ 39822 MiB/s, ...`.
That is all threads' bytes divided by the slowest thread's time on the test.

- **The per-thread table** gives each thread's Speed, and Dev S, its difference from the
  average.
- **`Config:`** names the pattern, as in `mode=TMR-10`.
- **`Intermediate:`** shows the work outside the figure. The pattern fill is a Bench-Verify's
  first write. In a sealed run, the seal check is the check of the sealed memory the step
  overwrites.
- **The final summary's per-test table** gives each test's Time, Data and Speed, averaged over
  its runs.
- **The results file:** `cycles[].tests[].throughput_mib_s` for each cycle, and
  `overall_stats.per_step_averages[].avg_throughput_mib_s` over the run.
- **Comparing runs:** `tmr --compare-results <one> <other>`, or the window's Compare tab, compares
  two runs test by test, and flags a test that ran at another width.

### Things to know

- **Compare like with like:** the same `memory=`, `cpus=`, `skip-cores=`, `width=` and page sizes
  (large pages, which TMR takes when it can). Any of them can move the figures.
- **Leave out `pattern-mode=`.** A numbered mode given there replaces the numbered Bench tests'
  own modes.
- **They aren't a stability screen.** Bench-Init checks nothing, and each Bench-Verify writes its
  pattern once and then only reads it back. To look for errors, run the stability plan.

Back to [TMR's tests](README.md).
