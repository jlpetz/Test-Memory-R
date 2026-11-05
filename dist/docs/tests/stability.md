# The stability tests

These tests look for memory errors: a bit that reads back different from what was written. Each
writes patterns, reads them back and counts every word that differs. Stable memory makes no
errors at all, so one error fails the run.

A plain `tmr` runs them: `plan=stability`, 3 cycles of about 5 minutes each. Three more tests in
the plan, Mem-PageExchange, Mem-PageExchange-Rewrite and Mem-RefreshAuto, are on
[their own page](refresh-and-exchange.md).

## At a glance

| Test | What it looks for | A step, by default | Plans |
|---|---|---|---|
| Mem-StuckBit | bits stuck at 0 or 1 | one pass over all memory | quick (Auto), stability (Auto), widths, all |
| Mem-SimpleV2 | TM5's SimpleTest: faults that depend on the data | 30 s | quick (Auto), stability (Auto), widths, all |
| Mem-SimpleNT | the same, its writes going straight to DRAM | 30 s | quick (Auto), stability (Auto), widths, all |
| Mem-MovingInv | a write that disturbs other cells, or lands at the wrong address | 30 s | quick, stability, all |
| Mem-ModuloX | cells disturbed by writes to the lines around them | 30 s | quick, stability, all |
| Mem-Sequence | reads and writes mixed, lines in a scrambled order | 30 s | quick, stability, all |
| Mem-Random | reads at random addresses, then every word once | 25 s, then a sweep | quick, stability, all |
| Mem-MirrorV2 | TM5's MirrorMove: swaps in place | 10 s | quick (Auto), stability (Auto), widths, all |
| Mem-Stride | accesses 8 B to 32 KiB apart | one pass | quick, stability, all |
| Mem-BlockMove | block copies: a read and a write stream at once | 20 s | quick, stability, all |
| Mem-CacheBust | writes a page apart, mostly inside the caches | 20 s | all |
| Mem-StuckBit-FlushAuto | Mem-StuckBit, each check forced to DRAM | one pass | all |

The stability plan runs the first ten in this order, Mem-StuckBit first so a hard fault shows in
seconds. `plan=quick` runs them once, `plan=all` runs every test, and `plan=perf` none of these.
`test=Mem-MovingInv` runs one test alone, with its own settings, for one cycle unless you give
`cycles=` or `duration=`. A timed step stops at the end
of the pass it is on, so it runs a little over when a pass over all memory takes seconds. The
one-pass tests take longer with more memory.

## How they check

- **Write, then read back.** Each thread tests its own memory, in chunks. Most tests build a
  word's value from its position, its thread and the pass, so a write that lands at the wrong
  address shows.
- **A failed check is rescanned at once,** before the test writes over the chunk. The rescan reads
  the chunk word by word, counts each bad word and logs the first 10, rereading the first 4 (also
  after flushing them from the cache). A rescan that finds every word right logs "not reproduced
  on rescan" and still counts one error: the check did see a difference.
- **The seal (TM5's test 0).** Each cycle starts by writing a pattern, the seal, over all the memory
  under test. A test checks each chunk still holds it before working the chunk, and writes it back
  after. So each chunk sits for about a step while the other tests hammer the rest of memory: that
  catches cells that lose their charge under load, and a test that wrote outside its chunk. A
  "Final seal check" of all memory ends the cycle. The seal's errors are counted apart. Mem-Random
  isn't sealed: its memory's seal is checked before it runs and written back after. More on
  [the seal](seal-and-bench.md).
- **`errors=halt`** stops the run right after the check that failed, before the test writes over
  the chunk, so the bad words stay in memory. By default a run logs each error and goes on.

## Widths: 128, 256, 512 and Auto

Mem-StuckBit, Mem-SimpleV2, Mem-SimpleNT and Mem-MirrorV2 come at three SIMD widths, the size of
each load and store: 128 bits (16 bytes, SSE2), 256 (32 bytes, AVX2) and 512 (64 bytes, AVX-512).
The width ends the name: Mem-StuckBit256, Mem-SimpleV2-256. Auto runs the widest the CPU has: 512
bits with AVX-512 (F, BW, CD, DQ and VL), else 256, or the one `width=` gives. It is shown under
that width with `_A` added: on an AVX-512 CPU, Mem-StuckBitAuto shows as `Mem-StuckBit512_A`.

The stability plan runs the Auto tests. `plan=widths` runs every width side by side, so each tests
the same memory in the same cycle: compare widths there, as separate runs drift. A CPU without
AVX-512 leaves the 512-bit tests out, and says so; they never run at another width.

Mem-Sequence picks its width too, 512 or 256 bits; it has no 128, so `width=128` runs it at 256.
Mem-MovingInv, Mem-ModuloX, Mem-Random, Mem-Stride, Mem-BlockMove and Mem-CacheBust have no width
variants.

## The tests

### Mem-StuckBit128/256/512/Auto

A stuck bit reads the same, 0 or 1, whatever was written. Each chunk is written with
`0xAA55AA55AA55AA55` and checked, written with its inverse `0x55AA55AA55AA55AA` and checked, then
written with the first again and checked. Every bit holds a 1 and a 0, and the third phase
catches a bit that fails only on the way back. The chunks are 512 MiB, far larger than the
caches, so the checks read DRAM. A step is one pass: seconds on a desktop, longer with more memory.

**Mem-StuckBit-FlushAuto** takes chunks of a thread's share of its core's L2, and flushes each
from the cache (CLFLUSHOPT) before each check, so the check is sure to read DRAM. At the plain
test's 512 MiB chunks the data has left the cache anyway, so it is in `plan=all` only. It shows as
`Mem-StuckBit-Flush512_A` or `-Flush256_A`.

### Mem-SimpleV2-128/256/512/Auto

TM5's SimpleTest, its main test: write a pattern over a chunk, read it back. A TM5 `.cfg`'s
SimpleTest runs on Mem-SimpleV2-Auto. The patterns come from a program, a list of families, each
written as a pair, P and then ~P (every bit flipped), so each bit is checked holding a 0 and a 1.
The built-in program is `random, header, random, switch, random, quiet`:

- **random** mixes the bits, so every data line sees every short run of bits.
- **switch** flips every data line on every transfer, at the memory's transfer width: 32 bits for
  DDR5, 64 for DDR4, from the memory type the BIOS reports, or `beat=`.
- **quiet** holds one data line steady while every other switches.
- **header** is switch with words unique to their place, so a misplaced half-line shows.

Each pass over memory writes one pair to every chunk and takes the program's next family, under a
key of its own, so passes don't repeat each other's values. The chunks are 4 MiB, and a step runs
30 s or 100 passes, whichever comes first.

A TM5 config's SimpleTest writes two pairs a chunk, its Pattern Mode picking the program;
`patterns=legacy` runs TM5's own patterns instead. `pattern-mode=` runs one family alone, and
`seed=` replays a run's values on the same threads and memory.

### Mem-SimpleNT-128/256/512/Auto

The same write-then-check with non-temporal (streaming) stores: the writes skip the cache and go
straight to DRAM, without the read a normal store makes first. The reads are ordinary. Each word
holds its position XORed with a per-thread constant, the same on every pass, so in a sealed run a
write that never lands shows on the step's first pass, where the chunk held the seal. The chunks
are 4 MiB, and a step runs 30 s or 100 passes.

### Mem-MovingInv

Memtest86's moving inversions. Each chunk is written with P going up. Then, going up, each word is
read as P and written as ~P; then, going down, read as ~P and written as P; then read as P. Each
word is checked just before it is overwritten, while the words below it hold one value and those
above it the other, so a write that disturbs other cells, or lands at another address, shows. P
differs for every word and changes every pass. The chunks are twice the L3, so every pass reaches
DRAM. A step runs 30 s.

### Mem-ModuloX

Memtest86's modulo-20. In each chunk every 20th 64-byte line is a victim: the victims are written
with P, every other line is written with ~P twice over, then only the victims are read. A victim
holds still while the lines around it are rewritten, so a cell disturbed by its neighbours' writes
shows. The victims move on a line each pass, from a random line each step, so in time every line
takes its turn. The chunks are twice the L3, and a step runs 30 s.

### Mem-Sequence

A list of passes over each chunk: `W` writes the program's next pattern, `R` checks it, and `RW`
checks each line and writes its inverse in the same visit, so the reads are checked with writes
mixed in. A pass goes forward, in reverse, in random order (every line once, scrambled anew each
pass, which the CPU's prefetcher can't predict) or in TM5's stride. The built-in list is
`W 2R RW 2R@random`, twice a chunk, with Mem-SimpleV2's program. The chunks are twice the L3, and
a step runs 30 s.

`sequence=` and `order=` change it (`sequence=W,4R,RW@random,R`, `order=reverse`): up to 16
steps, each with an optional count before it and `@order` after it. A numbered `pattern-mode=`
leaves Mem-Sequence alone: it runs the native patterns only.

### Mem-Random

It fills its memory once, each word with its own ID. Each pass then reads as many words as the
memory holds, at random addresses (8 random sequences), and checks each against its ID. Reads can
land on a word twice, so a pass leaves about a third of the words unread; after the last pass a
sweep reads every word once, in order, catching one the reads missed or one that decayed while
the test ran. A step is 25 s, then the sweep; a pass over a large memory takes seconds, so the step
runs longer. Its MiB/s is low by nature: one 8-byte read at a time, from anywhere.

### Mem-MirrorV2-128/256/512/Auto

TM5's MirrorMove. The chunk's two ends walk towards each other, swapping, and cross the middle, so
every pair is swapped twice and the chunk ends as it began; then it is checked. Its data is the
seal, so its check is a seal check, whose errors count as the mirror's own. The chunks are 8 MiB,
and a step runs 10 s.

| Variant | Memory a thread | Mirror |
|---|---|---|
| -Auto, -128 | 64 MiB | the whole chunk |
| -256 | 128 MiB | 2 parts, in step |
| -512 | 256 MiB | 4 parts, in step |

`mirror=` sets every mirror test's mirror (`whole`, `subblocks:2`, `subblocks:4`, or `jump:N`, TM5's
MirrorMove128), and `test-reps=` the round trips before each check. A TM5 config's MirrorMove and
MirrorMove128 run on Mem-MirrorV2-Auto.

### Mem-Stride

Six passes over all memory, each at a distance: every word, then every 16th, 64th, 256th, 1024th
and 4096th (8 B to 32 KiB apart), each 2 MiB chunk in 4 parts. The distances switch the memory's
banks and rows in different ways. A step is one round of the six passes.

### Mem-BlockMove

It copies 8 MiB pieces from the first half of its memory to the same place in the second half,
then checks the copy: a read stream and a write stream at once. Each thread's memory is twice the
caches (every core's L1 and L2, plus one L3), so the copies stream through DRAM. A step runs
20 s. A TM5 config's BlockMove runs on it, and a JSON config's `"copy_directions"` sets the copy's
directions (1, forwards, by default).

### Mem-CacheBust

It writes each 1 MiB chunk a page (4 KiB) apart, the first word of every page, then the second,
and so on, and checks it the same way. Every access is to another page, so every one misses the L1
cache. Its memory is half the caches, so it works mostly inside them, which is why the stability
plan leaves it out. A step runs 20 s.

## Reading the output

Each step ends with a report. A failed one (an example):

```
📊 Test report - Cycle 1 - Step 2 Mem-SimpleV2-256_A: ❌ 31.4s, 1 error (1 test + 0 seal + 0 WHEA), Cycles 🟢 9/100, Time Limit 🔴
   Failing bits: test (bit 17)
   Test: 31.2s, 1104.00 GiB @ 36233 MiB/s, ...
   Config: parameter=-, mode=random+header+random+switch+random+quiet, extent=FullAllocation, chunk=Absolute (4.00 MiB), test_reps=1, verify_reps=1, wrc=2
```

- **✅ or ❌** is the verdict. Any error fails the step: the test's own, the seal's, or a WHEA event
  (a hardware error Windows recorded during the step).
- **Cycles 9/100** counts the test's own passes in this step, 100 its limit (`∞` for none). They
  aren't the run's cycles, which repeat the whole plan. 🔴 marks the limit that ended the step.
- **Failing bits** are the bit positions (0-63 of the 64-bit word) that failed.
- **Test** is the time, data and MiB/s. Each test counts its own traffic (Mem-StuckBit three writes
  and three reads a chunk), so compare a test with itself in another run, not with another test.

The table under it has a row a thread: its CPU, core, time, data, speed, passes and errors. Each
thread tests its own memory, so the row says whose memory an error was in.

Each bad word is logged, on the console and in the log file:

```
Error found by Mem-StuckBit256 in step 1 (Mem-StuckBit256_A), cycle 1, thread 0, idx 1000 (address 0x...): expected 0x55aa55aa55aa55aa, got 0x55aa55aa55a855aa, XOR 0x0000000000020000 (1 bits)
```

`idx` is the word's place in the thread's memory, and the XOR shows which bits were wrong. Then a
line for the failed check says what the rescan found: `1 bad words on rescan`, or `not reproduced
on rescan`.

The final summary splits the errors into Test, Seal and WHEA, for the run and for each test, and
adds an **Errors by Step** table (the final seal checks as `end`) and a **Failing Bits** table:
each bit's failed checks and bad words, and which way they went (0→1 + 1→0).

The results file has every step of every cycle: `errors`, `alarms` (the checks that failed),
`not_reproduced`, `failing_bits`, `seal_errors`, `seal_failing_bits`, `whea_total` and
`stopped_by` (`cycles`, `time` or `stopped`), each thread's errors too, and each cycle's
`seal.final_check_errors`. `tmr --compare-results` compares two runs.

## Settings that matter

Beside each test's own settings above:

| Setting | What it does |
|---|---|
| `cycles=`, `duration=` | how many cycles, or how long |
| `width=256` | the width of the Auto tests, Mem-Sequence and the seal |
| `seal=off`, `seal=tm5` | no seal, or TM5's seal pattern (`tmr` by default) |
| `rescan=off` | one error a failed check, no rescan: for runs with very many errors |
| `write-read-cycles=`, `verify-reps=` | write-and-check rounds a chunk, and checks after each write |
| `memory=`, `cpus=` | how much memory, and how many threads |

`tmr --help` explains each.

## Caveats

- **TMR doesn't know where a word sits in the DRAM chips** (which bank, which row). Mem-Stride's
  distances and Mem-ModuloX's neighbours are in the test's memory, not in the chip.
- **A small `memory=`** shrinks the chunks to each thread's memory, where they may fit in the
  cache, which weakens Mem-StuckBit's checks.
- **An error can count twice.** Where the chunk size doesn't divide a thread's memory, the chunks
  overlap a little, and a bad word in the overlap is found by both.
- **`test=Mem-StuckBit256` also runs the Auto test** where Auto picks 256 bits, as both run as
  Mem-StuckBit256. `test=Mem-StuckBitAuto` picks the Auto one alone.

Back to [TMR's tests](README.md).
