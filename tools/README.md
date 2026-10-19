# Kernel tools

Checks for what tests and clippy can't see: what the compiler made of a hot loop, and whether a
change made a test faster. Both came out of TODO 99; the rules they check are Rule 6 of
`doc/simd_codegen_rules.md`. Keep them working when the code they read changes (a new test module,
a new results field), and add to them when a round finds a new trap.

## `hot_loop_spills.py`: what the hot loops do that they shouldn't

Reads the post-LTO asm and lists, per kernel, the innermost loops that touch the stack: values the
register allocator couldn't keep in registers, split into round trips (a slot stored and reloaded,
on the loop's dependency chain) and reloads (a constant read again each iteration), each for
general-purpose and SIMD (vector and mask) registers. Two more columns count loops for review:
`indexed` (an access through an index register rather than a walked pointer) and `const` (a
constant read from the binary's data every iteration).

```bash
cargo rustc --release --bin tmr -- --emit asm      # the real binary's code; --lib is pre-LTO
python tools/hot_loop_spills.py                    # the table, worst kernels first
python tools/hot_loop_spills.py --detail cache_busting --all   # one kernel's loops, clean ones too
```

Run it before and after touching a hot loop, and save both `--detail` outputs: the loop bodies are
also the place to check the width and the instructions.

**Limits.** It decides "hot" by shape: an innermost loop that makes no call and touches memory off
the stack. Some cold loops pass that test and count: per-pass or per-chunk setup (a division, the
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
counts the kernels whose hot loops are all clean.

## `bench_ab.py`: A/B speeds, alternating

Runs two or more binaries in turn (A B A B ...) and compares each test's MiB/s from the results
files, raw and relative to a reference test the change doesn't touch (the box's state cancels).

```bash
python tools/bench_ab.py OUTDIR Mem-StuckBit512_A \
    "test=Mem-CacheBust,Mem-StuckBitAuto cycles=2 memory=50% cputype=cores cpus=100% skip-cores=0" \
    before=path/to/before.exe after=path/to/after.exe --runs 3
```

Copy each binary out of `target/` before measuring; the next build replaces it. Build nothing while
it runs. For the noise floor, run one binary under two labels (an A/A run): on the dev box two runs
of identical code landed 1.0–1.6% apart (2026-10-19).
