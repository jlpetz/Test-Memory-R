# Mem-TileTransform: the AMX tile test

Mem-TileTransform tests your memory while the CPU's matrix unit works on it. It runs only on Intel
Xeon processors that have AMX (Advanced Matrix Extensions). On every other CPU TMR leaves it out of
the run and says why. It is experimental, and only `plan=all` runs it, or you name it.

## What it's for

Some memory settings pass while the CPU is idle and fail once the cores are busy and the system is
warm. Most of TMR's tests only move data: they write a pattern, read it back and compare. This test
runs the memory through the matrix unit as part of the test itself: every tile it reads goes
through a multiply. It catches:

- **Bits that flip in memory**, as any TMR test does, with the address, the tile, and the byte and
  bit that flipped.
- **When the bit flipped**: before the multiply (the pattern went bad in memory) or after it (the
  result went bad). The two leave different marks, and TMR names which one it saw.
- **A matrix unit that miscomputes.** Every multiply's result goes into the check, so a wrong
  product shows up as a mismatch too. Before a step tests any memory, it also checks the matrix
  unit against a result stored in TMR.

TM5 has no test like this one, so a TM5 config never runs it.

## Which CPUs run it

AMX is in Intel Xeon processors from the 4th generation Xeon Scalable (Sapphire Rapids) on, and in
the Xeon W workstation chips of that family. No desktop Core processor and no AMD CPU has it. TMR
checks for it at startup: AMX-TILE and AMX-INT8, Windows' support for the tile registers, and tile
sizes the test can use.

Where the CPU lacks it, a run that includes the test (`plan=all`, say) skips it and prints a line
like this, then runs everything else:

```
ℹ  Left out 1 test(s) this CPU can't run (the tiles need AMX-INT8 and the OS's tile state, which this machine lacks): Mem-TileTransform
```

If you name only this test, nothing is left to run, and TMR stops before testing. A virtual machine
can hide AMX even when the CPU underneath has it. If the line appears on a Xeon that should have
AMX, that's the first thing to check.

## How it works

A **tile** is 1 KiB of memory: 16 rows of 64 bytes, which is 16 cache lines in a row. The test loads
and stores whole tiles with the matrix unit's own instructions, one tile per instruction, using
ordinary cached stores.

A **group** is 16 tiles in a row (16 KiB) by default. The test has a set of 16 pattern tiles: all
zeros, all ones, the two checkerboards (`0xAA…` and `0x55…`), walking ones, walking zeros, and ten
pseudo-random tiles. Each group holds every pattern once. Which tile holds which pattern changes
with every pass. Each step of each cycle, and each thread, also arranges the patterns differently,
starting from the run's `seed=`. That way two groups seldom hold the same data, which could hide an
addressing fault between them.

It takes each chunk of a thread's memory through three phases, and each phase walks the whole chunk:

| Phase | What it does |
|---|---|
| **W** (write) | Stores each tile's pattern. |
| **RW** (read, transform, write) | Loads each tile, multiplies it by a fixed matrix, and stores the result back in place. Memory now holds a transformed pattern, different bit for bit from what W wrote. RW reads four tiles ahead to keep DRAM busy. |
| **R** (read and check) | Loads each tile, multiplies it by its pattern's own check matrix, and adds the product into a running checksum for its group. The test then compares that checksum with the value every clean group gives. |

So every byte of the chunk is written twice with different data and read twice. A chunk's ends can
cut through a group, because chunks start on 4 KiB boundaries and groups are 16 KiB or more. The
tiles of a partial group at either end of a chunk are compared word by word instead.

The checksum is built so that **any one or two flipped bits** in a group's stored result change it,
at every `tile_load` setting. For one bit flipped in a pattern before RW reads it, TMR's own tests
check the built-in set exhaustively: every bit of every pattern is caught. Three or more flips in one
group could in theory cancel out, but only with an exact arithmetic relation between them. TMR's
tests corrupt groups with 3 to 16 random flipped bits, or with random masks over 1 to 8 words, and
catch every case.

**Before each step**, every thread checks its matrix unit. It recomputes the built-in pattern set's
checksum on the tiles and compares it with a copy stored in TMR. If they differ, the CPU is at
fault, not the memory: the thread logs one error and ends its step before it tests any memory.

When a group's checksum is wrong, TMR rescans the group word by word, as its other tests do. It
logs each bad word, rereads the first few, and reports "not reproduced" when every word reads back
right. Then it recomputes the group's checksum through the tiles again. That second read goes
through the tile path, not ordinary loads.

## Its memory and its chunk

- **Extent:** all of each thread's memory.
- **Chunk:** twice the L3 one thread reaches. By default that's one L3 group's L3 as the CPU
  reports it. If `tmr-cfg.json` holds this machine's cache calibration, it's the measured size.
  Either way the chunk is never more than the thread's memory, and it's rounded up to 4 KiB. On a
  Xeon whose L3 is 105 MB, for example, chunks are about 210 MB a thread. This is the same chunk
  Mem-MovingInv uses. It's large so that each phase works from DRAM, not the cache.
- The chunks are spread evenly over the thread's memory. When the chunk size doesn't divide the
  memory, they overlap a little. The plan table's Chunk column shows the size and the count, with
  `(DRAM*2)` after them.

Like TMR's other correctness tests, it takes the seal, TM5's test 0. TMR writes the seal over all
memory at the start of each cycle and checks it at the end. Before this test works on a chunk, it
checks that the chunk still holds the seal. After the chunk, it reseals it.

## Variants

There is one test, Mem-TileTransform. It has no 128, 256, 512 or Auto variants, and `width=`
doesn't change it: the matrix unit does all its work. Its two settings, `tile_patterns` and
`tile_load` (below), only come from a TMR `.json` config.

## Which plans run it, and for how long

| Plan | Runs Mem-TileTransform? |
|---|---|
| `stability` (the default), `quick` | No |
| `widths`, `perf` | No |
| `all` | Yes, after Mem-MovingInv, in each cycle (3 cycles unless you give `cycles=`) |

Run it by name with `tmr test=Mem-TileTransform`, which runs one cycle unless you give `cycles=` or
`duration=`.

A step runs for **30 seconds**. The test only checks the time at the end of a whole pass over the
thread's memory, so a step always does at least one full pass. On a large memory it can run past
30 s by up to one pass. Ctrl+C doesn't wait for the pass: the test stops during a chunk's check, or
at the end of the chunk.

## Reading its output

### The step report

The report after each step reads as it does for TMR's other memory tests:

- **The header line**: the step's time and its errors, split into `test + seal + WHEA`.
- **`Test:`**: the time, the data moved and its speed in MiB/s. The test counts each byte of a
  chunk **four times** (W writes it, RW reads and writes it, R reads it). So its MiB/s is memory
  traffic, which is more than the speed at which it covers your memory. Its `ops` are that traffic
  in 8-byte words.
- **`Intermediate:`**: the seal's checks and reseals around the chunks. They count separately from
  the test's own time.
- **`Config:`**: `parameter=-` with the built-in settings, or `Tiles(32,r8)` when a config sets
  `tile_patterns` (32 here) or `tile_load` (8). The `extent` and `chunk` fields show the memory
  described above.
- **The thread table**: each thread's time, data, speed, passes (the `Cycles` column) and errors.

### Errors

The **Errors** column counts the bad words the rescans found. A check whose rescan found every word
right counts one error, "not reproduced": the check saw a difference that didn't come back. So a
transient read error is counted too.

The **Seal** column counts the errors the seal check found in a chunk just before this test wrote
over it. That memory went bad after an earlier step sealed it, and this test didn't cause it.

Each error writes lines to the console and the log (the window's Log tab). After the usual
per-word lines (`idx …: expected …, got …, XOR …`), the test adds a line for each bad tile:

```
Error found by Mem-TileTransform in step 1 (Mem-TileTransform), cycle 1, thread 3, tile 1234 (idx 157952..158080, pattern 7): 1 bad words, one stored bit: byte 77 bit 3
```

`pattern` is the tile's pattern in the set: 0 is all zeros, 1 all ones, 2 and 3 the checkerboards,
4 walking ones, 5 walking zeros, and 6 or more the pseudo-random ones. The end of the line says what
the damage looks like:

| It says | What happened |
|---|---|
| `one stored bit: byte B bit b` | One bit of the transformed result went wrong after RW stored it, or one read of it went wrong. `B` is the byte in the tile (0 to 1023). |
| `one bit flipped before the transform: pattern byte B bit b, 0 to 1` (or `1 to 0`) | The pattern W stored went bad before RW read it. That one bit changes a whole 64-byte row of the result, and TMR works back from the row to the pattern's byte and bit, and the way it flipped. |
| `not one bit` | More than one bit, or neither shape. |

After the tile lines come the rereads of the first few bad words. Then a line gives the record
every TMR check leaves: how many bad words the rescan found, and their differences ORed together.
For this test, the line's "the check's differences, ORed" always reads `0x0000000000000000 (0
bits)`, because a checksum found the error, not a word-by-word compare. Look at the bad words' own
differences earlier in the same line.

For a whole group, one more line rechecks the group's checksum through the tiles:

- `still wrong` after bad words were found: the memory is bad, as the rescan said.
- `still wrong` after nothing was reproduced: the tile path sees an error the word reads don't. That
  points to the tile loads or the multiply, not the memory cells.
- `right` after nothing was reproduced: the first read was wrong once and the memory was fine.

With `rescan=off` there's none of this. Each failed check counts one error, and its line names only
the tiles it covered.

If instead you see `the matrix unit computed the built-in pattern set's checksum wrongly: a fault in
the CPU, before any memory was tested`, the matrix unit failed its check at the start of the step.
Treat it as a CPU or CPU-voltage problem, not a memory one.

### The summary and the results file

The final summary's per-test row, the **Errors by Step** table and the **Failing Bits** table work
as they do for every test. One caveat on the Failing Bits table: it counts the bits that differ in
the stored words. For a flip before the transform, those are the bits of the transformed row, which
are many. The bit that actually flipped is the one in the tile line.

In the results file (`results\TMR_<date>_<time>.json`), the step has:

- `errors`, `alarms` (checks that found a difference) and `not_reproduced`;
- `failing_bits` and `seal_errors`;
- `cycles_completed_min` and `cycles_completed_max` (its passes);
- `bytes_processed` (counted four times, as above) and `throughput_mib_s`;
- the same counts for each thread.

## Settings

The two settings only come from a TMR `.json` config, as a `test_sequence` entry. No command-line
option sets them.

| Setting | Values | What it does |
|---|---|---|
| `tile_patterns` | 16 (default), 32, 64 | Patterns in the set, which is also the tiles in a group (16, 32 or 64 KiB). 16 is the built-in set, whose checksum TMR stores. 32 and 64 come from the same seed, and TMR computes their expected values on the tiles. |
| `tile_load` | 0 (default) to 65534 | Extra multiplies per tile in R. Each one adds the same product into the same checksum again, so every product is still checked, and errors are caught the same way at every value. It makes the matrix unit busier and the test slower. |
| `flush_before_verify` | `false` (default), `true` | Flushes each chunk out of the cache after W and RW, so R reads it from DRAM. At twice the L3, most of a chunk has left the cache by then anyway. |

`tile_load` is how you add CPU load. At 0, the matrix unit is about 10% busy during R at DRAM
speed. In the prototype the test came from, on a Xeon 6975P-C, `tile_load` 8 cost a pass about 20%
of its speed, and the unit was busy for about half the pass. TMR refuses a value above 65534 (past
that, the checksum could miss a one- or two-bit fault) and any `tile_patterns` but 16, 32 or 64.
Any other test refuses both settings.

To run it from a config, start from one of the examples in `configs\` (`tmr --create-demo-configs`
writes them again), and put an entry like this in its `test_sequence`:

```json
{
  "enabled": true,
  "function": "Mem-TileTransform",
  "tile_load": 8,
  "extent": { "mode": "full_allocation" },
  "chunk": { "mode": "cache", "target": "DRAM*2" },
  "cycles": 1000,
  "duration_secs": 30
}
```

An entry in a config takes the config's default extent, chunk and timing unless it sets its own.
The `extent` and `chunk` here match the built-in plans. With both `cycles` and `duration_secs`, the
step ends at whichever comes first, so this one runs about 30 seconds, as the plans do. Leave
`cycles` out and the entry takes the config's `default_test_cycles` instead. The demo configs set
that to 1, which is one pass here.

## Caveats

- **Xeon only.** On a CPU without AMX it never runs, so a clean run there hasn't tested this.
- **Experimental.** TMR hasn't yet timed it against its other tests, and its settings may change.
- **The load comes in bursts.** `tile_load` adds multiplies to R only. W does none and RW does one
  per tile, so even at a high setting the matrix unit is idle for part of each pass.
- **Its MiB/s is traffic**, each byte counted four times, so it doesn't compare with another test's.

Back to [TMR's tests](README.md).
