# Kernel and console tools

Checks for what tests and clippy can't see: what the compiler made of a hot loop, whether a change
made a test faster, and whether a change meant to move code left the kernels' instructions and the
console's text as they were. The first two came out of TODO 99, and the rules they check are Rule 6
of `doc/simd_codegen_rules.md`; the last two came with TODO 29's engine/presentation split. Keep
them working when the code they read changes (a new test module, a new results field, a new line on
the console), and add to them when a round finds a new trap.

## `hot_loop_spills.py`: what the hot loops do that they shouldn't

Reads the post-LTO asm and lists, per kernel, the innermost loops that touch the stack: values the
register allocator couldn't keep in registers, split into round trips (a slot stored and reloaded,
on the loop's dependency chain) and reloads (a constant read again each iteration), each for
general-purpose and SIMD (vector and mask) registers. Two more columns count loops for review:
`indexed` (an access through an index register rather than a walked pointer) and `const` (a
constant read from the binary's data every iteration).

```bash
cargo rustc --release -p test-memory-r --bin tmr -- --emit asm   # the real binary's code; --lib is pre-LTO
python tools/hot_loop_spills.py                    # the table, worst kernels first
python tools/hot_loop_spills.py --detail cache_busting --all   # one kernel's loops, clean ones too
```

Run it before and after touching a hot loop, and save both `--detail` outputs: the loop bodies are
also the place to check the width and the instructions.

The binary is `crates/tmr-app`'s since TODO 29 S3, its package `test-memory-r` since TODO 104,
hence `-p test-memory-r`; its asm lands under `target/release/build/test-memory-r/`, which the
tool's glob finds. The old command, `cargo rustc --release -- --emit asm`, still succeeds, but
builds the lib's asm (`build/test-memory-r-engine/<hash>/out/tmr-<hash>.s`), pre-LTO, whose kernels make a believable table. So both
tools look for that file too, and refuse an asm with no `main` (exit 1), the newest one included,
rather than read it or pass it over for an older `tmr.s`. A table with no hot loop would read as a
clean one, so finding none fails it too: the symbols are no longer the `tmr` crate's (`3tmr`), or
`MODULES` no longer its modules.

**Limits.** It decides "hot" by shape: an innermost loop that makes no call and touches memory off
the stack (one that calls is reported below the table, by why). Some cold loops pass that test and count: per-pass or per-chunk setup (a division, the
shutdown flag), an inlined rescan, a setup fill inlined into the test function. Read the loop
before acting on it. Functions are left out by name (`COLD`), so a new cold helper with a
different name shows up until it is added. It can't see:
- a value reloaded through a pointer register rather than `%rsp` (a closure's captures pinned in
  memory, Rule 6.10), which reads as a clean loop;
- a loop whose skip path forms a smaller backward branch: only the skip loop is innermost, so the
  body isn't classified;
- the loop's shape (Rule 6.5), or the order it walks memory in. The `width` column checks the
  register width against the kernel's name, but only for kernels whose name has one; the Spd tests'
  untimed setup fill (inlined, vectorized to ymm) shows there in their 128- and 512-bit functions.

Fixed in round 3 (2026-10-19): stack arrays indexed by a register (`128(%rsp,%r10)`) count as
stack accesses; loops up to 600 instructions are classified (over 150 flagged `long`, where the
cap used to hide them); indexed prefetches don't count as indexed; epilogues laid out inside a
loop's range are skipped; const-generic instances are named (`verify_strided::<16>`); and a line
counts the kernels whose hot loops are all clean. A kernel generic over the access a unit test
records it by (`tests::access`; only `Direct` is in the binary) is named without it, as it was
before it took one (2026-10-20).

Since TODO 101 (2026-10-24), a hot-shaped loop that makes a call, which isn't in the table, is
reported below it rather than dropped without a word: a hot loop that picked up a call (an
intrinsic that didn't inline, a helper kept out of line, a copy or fill loop LLVM collapsed into
`memcpy` or `memset`) would have left the table looking cleaner. Each is counted by why: timing,
an error path, bookkeeping or allocation (`COLD_CALLS`), dispatch (a test function with a loop of
its own, or a few calls below, the loop handing it a chunk or a phase), or a struct move (a
`memcpy` of a constant size). The rest are listed by kernel and callee, to read. On the asm of
2026-10-24: 440 such loops, 14 to read (indirect calls in the mirror moves and the numbered
SimpleTest sequences, hashing in Mem-Random's rescan and the latency tests' setup, and the
rescans' own); the table above them was unchanged.

## `bench_ab.py`: A/B speeds, alternating

Runs two or more binaries in turn (A B A B ...) and compares each test's MiB/s from the results
files, raw and relative to a reference test the change doesn't touch (the box's state cancels).

```bash
python tools/bench_ab.py OUTDIR Mem-StuckBit512_A \
    "test=Mem-CacheBust,Mem-StuckBitAuto cycles=2 memory=50% cputype=cores cpus=100% skip-cores=0" \
    before=path/to/before.exe after=path/to/after.exe --runs 3
```

Copy each binary out of `target/` before measuring; the next build replaces it. Build it plainly
(`cargo build --release`): under `cargo build --all-targets` tmr.exe links the engine with the
`test-support` feature that tmr-cli's tests turn on (Cargo builds a crate once a command, with
every feature its targets ask for). Build nothing while it runs. For the noise floor, run one binary under two labels (an A/A run): on the dev box two runs
of identical code landed 1.0–1.6% apart (2026-10-19).

## `asm_fingerprint.py`: whether the kernels' instructions changed

Hashes each function of the test code in the post-LTO asm: every one of `hot_loop_spills.py`'s
`MODULES`, its `COLD` ones too (a kernel's rescan, which formats the error lines, is off the hot
path but still the test's), and the workers' side of a step, `thread_pool` (the worker loop,
`WorkItem`, `WorkResult`) and `test_scaffolding` (`TestRunner`'s bookkeeping). Each is hashed
after normalising what moves when the code doesn't: label numbers and string constants are
renumbered in their order of use; LLVM's numbered copies of a symbol (`<sym>.449`) lose the number
when they are a function's (a call reaching one copy or another is the same call), and are
renumbered in their order of use, each symbol's apart, when they are data's; v0 symbols lose their
crate hashes, back-references and the crate a generic was instantiated in, a closure its place
among its function's closures, and a COMDAT section its number in the file. The data a function
names (a constant pool, a jump or switch table, a string) is hashed with it, each number in it
spelled one way (decimal, unsigned). A change that
only moves code around the kernels, as each stage of TODO 29 does, must leave every hash as it was,
but for source lines: a log record's line inlined into a function is an immediate, `(line << 32) |
1`, so each function also has a hash with those masked, and `--compare` lists one that differs
only there as `lines`, which doesn't fail it (TODO 29 S1, where lines added to `runner.rs` showed
in the worker loop).

```bash
cargo rustc --release -p test-memory-r --bin tmr -- --emit asm
python tools/asm_fingerprint.py --save before.json      # on the old code
python tools/asm_fingerprint.py --compare before.json   # on the new: changed, gone, new (1 if any), lines
python tools/asm_fingerprint.py --dump DIR              # each normalised body, to diff two builds
```

Save the old code's fingerprint before building the new, and keep a copy of its asm: the build
replaces the file. The command builds the window in (tmr-app's default feature since TODO 7, as
each release is), so compare its asm with a stage's `_gui` fingerprints; the CLI alone's asm,
`cargo rustc --release -p test-memory-r --bin tmr --no-default-features -- --emit asm`, compares with
the `_cli` ones. When you build both, rename each asm file before building the other: both tools
read the newest `tmr.s` (the window's symbols name `windows_reactor`, the CLI build's none).
Compare builds of one toolchain: a nightly bump changes the code itself. The
saved file records the compiler the asm names, and `--compare` warns when the asm is another's
and then exits 2 rather than 1 on a change. TODO 29's baselines, at 25ff780, are in
`doc/todo/29-baselines/` with the spill table of the same build, and from S3 on each stage's own
fingerprints, CLI and GUI (`asm_fingerprint_s4_cli.json`), which the next stage compares against.

Since release builds carry line tables (`debug = "line-tables-only"`, d54fb4c), LLVM puts a `.Ltmp`
label at almost every instruction, for the debug sections; the tool drops those that nothing in
the function names. Checked on 2026-10-24: one tree built with line tables and without compares
0 changed of 511 names (before the fix, 511 changed). A baseline saved before the fix kept one
such label, inlined from std, in `thread_pool::ThreadPool::new`, so against any of those that
function alone reads as changed.

Checked on 2026-10-22: a rebuild with every crate hash changed (`RUSTFLAGS` with a
`-C metadata=`) compares clean, its spill table too, and so does a build of 25ff780's own tree,
while a build of the code of 2026-10-16 shows 238 of the 278 names both have changed (one read by
hand: a different stack frame). Before that check, the normalisation was short of the local copies,
the instantiating crate, and the statics `hot_loop_spills.py` reads as functions (left out here: a
static's body is the data after it). TODO 29 S1 (2026-10-22) added the closures' places, the COMDAT
numbers and the source lines, after three functions read as changed with none of their
instructions changed; the baseline was saved again from a build of 25ff780's tree, and another
build of that code compares clean with it. TODO 29 S3 (2026-10-23), when the bin moved to its own
crate, made function copies lose their numbers and data copies count per symbol: calls reached the
lib's copy of `<String as Display>::fmt` rather than another, and with one counter for every copy
a function named, the others' numbers moved with it. With the GUI linked in, a constant pool the
kernels share with Reactor's code was written in hex (`.quad 0x7fffffffffffffff`, a pool LLVM typed
as floats), hence the one spelling. Under the old normalisation S3's asm read 27 functions as
changed (32 with the GUI); under the new one, S2's `BusBeat::map_or` and the merged calls under
Limits (2 functions, 5 with the GUI), and the worker loop's closure as lines only. The baseline
was saved again from a build of 25ff780's tree, which compared clean against the old one under the
old normalisation, its spill table too; S2's build reads against the new one as its stage note
says.

**Limits.** It sees the asm, not where the linker puts it: two builds can be instruction-identical
and differ in speed by alignment, which is `bench_ab.py`'s to measure. A kernel inlined into a
function outside the modules isn't seen. Read the first diff it reports by hand (`--dump` both
builds and diff the two directories): a normalisation can hide a change or flag noise. A line in
a panic's location is in the function's data, as bytes of a string, and isn't masked: code moved
above an `unwrap` inlined into a kernel shows as a change. A call to a function LLVM merged with an
identical one names the one it kept, so a change far away shows: since S3 the thread pool's spawn
shim calls the backtrace frame of the Ctrl+C handler's thread, named after the function that sets
the handler (`tmr_cli::take_ctrl_c` since S5, in the CLI's crate, `cli::take_ctrl_c` in S4,
`cli::run_session` in S3, `cli::run` before), and in a build with the GUI three callers of
`RandomState::hash_one::<&u64>` call Reactor's identical `hash_one::<&RealizedContainer>`. Who else
calls a function can change how it is called: a generic is compiled again in each crate that uses
it, and LLVM folds an argument every caller passes the same into the function. In S5 the call
`ThreadPool::new` makes when a worker thread can't be spawned (std's `thread::spawn`, its
`expect("failed to spawn thread")`, to `core::result::unwrap_failed::<io::Error>`) passes two
arguments fewer: the engine's copy of that function lost its one caller with another message, the
Ctrl+C handler's thread (`Critical system error while waiting for Ctrl-C`), which tmr-cli's own
copy serves now, so the message is folded into the engine's. Those instructions, on a path never
taken, are the only ones that changed. A constant pool's bytes are written in the type of its first
user in the module, so code added elsewhere can respell a kernel's: in S4 the 128-bit SimpleTest
kernels' `__xmm@00000000000000010000000000000000` became `.zero 8`, `.quad 1` where it was
`.long 0`, `.zero 4`, `.long 1`, `.long 0`, the same 16 bytes, and 15 functions read as changed.

## `console_diff.py`: whether the console's text changed

Part of the console's output is pinned in process, by the CLI's goldens in
`crates/tmr-cli/testdata/console/` (`crates/tmr-cli/src/golden.rs`): each `Reporter` method from
fixed models, each event and how a session ends, the ticker, a cycle of `execute_test_cycle` (its
seal stages, step reports and the workers' lines), the plan `--startup-debug` prints, and the stops
before testing an override can reach. The engine's code the last three run is driven through
`tmr::test_support`, which the engine has in its own tests and under its `test-support` feature,
which only tmr-cli's tests turn on (TODO 29 S5). The one-shot commands' text is the engine's
golden, `testdata/console/commands.txt`. `TMR_BLESS=1 cargo test` rewrites both sets (the default
members; `--workspace` would also build the window and rewrite its `colours.example.json`), and
`git diff crates/tmr-cli/testdata/ testdata/` shows what changed. The rest rests on an A/B of the
real exe, diffed here: the setup path, which can't run in a test (secedit, `process::exit`); the
run around the cycle (WHEA, allocation, the seed, the cycle's start, the verdict, the results); the
line on the tests a CPU can't run, which depends on the CPU; a config's errors; and which stream a
line goes to, as the goldens take stdout and stderr together. Run the same command line under the
old binary and the new, each captured, then diff the two here.

Both captures are read the same way. Ticker frames are dropped (by their erase codes, or by their
lines when a capture lost the codes) and ANSI escapes stripped. Masked: log stamps, hh:mm:ss, run
stems, pattern seeds and addresses, and what a run measured: times, rates, op and sample counts and
every other decimal (deviations, latencies, ratios, frequencies, percentages). What a run is
configured with stays: byte counts, whole seconds (`30s`, a test's limit), a `Wait(500ms)` and a
cache multiplier (`CacheTotal 2.00x`). A run of lines naming a thread (the workers' `[Thread 3]
Running ...`) is put in thread order, since the scheduler orders them. A table's cells are joined by
` | ` (a masked cell no longer sets its column's width), and a table of threads has its rows sorted:
a step's thread table orders them by time, its latency table by when each thread finished.

```bash
cmd /c "tmr.exe --startup-debug memory=50% cputype=cores cpus=100% skip-cores=0 > before.out 2> before.err"
python tools/console_diff.py before.out after.out [--mask REGEX ...] [--context N]
python tools/console_diff.py before.err after.err
python tools/console_diff.py --show before.out      # one capture as it is compared
```

It exits 1 on a difference. Capture stdout and stderr to files of their own and diff each pair, so
a line that moves from one stream to the other shows. Capture from cmd.exe: Windows PowerShell 5.1
writes a native exe's stderr as error records that quote the command line, the exe's path with it
(a capture may still be UTF-8 or UTF-16, as PowerShell's `>` writes it). Capture both with the same
command line, on the same box, one right after the other.

**Limits.** The masks go by shape. A measured figure that looks like configuration shows as a
difference: the free memory in the memory table, a WHEA count, the pages an allocation got, and the
data and cycles of a step that its time limit ended (a cycle limit gives the same work every run).
Mask it with `--mask` once the diff shows it (`--mask "\d+\.\d+ [KMG]iB"` masks every byte count).
The other way, a configured decimal other than a cache multiplier is masked, so a change to it
doesn't show. Lines from two threads that aren't next to each other, or a log line landing inside
another thread's run, keep their order and can still differ. Sorting a thread table's rows hides a
change in their order.

## `icon/`: tmr.ico and the DIMM artwork

Not a check: the two scripts that draw TMR's icon and its artwork, a DDR5 DIMM in red neon, and
pack the icon that is copied to `crates/tmr-app/tmr.ico`. How to run them and what they assert:
`tools/icon/README.md`.
