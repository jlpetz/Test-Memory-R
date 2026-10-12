# The seed mask on Zen 3: the loops compared, and a loop-alignment 2x2 (2026-10-11 to 12)

The "seed mask" cut the `switch` hash to the nibbles a transfer width uses before spreading it into
bytes (Astra's follow-up, `doc/pattern_followup_review_2026-10-11.md`). It made the 256-bit write
loop shorter, but on a Ryzen 7 5700X (Zen 3, DDR4-3600, the 256-bit native kernels) the native
`switch` verify ran slower with it at 16 and 32 bits. It is reverted (TODO 97). This folder holds
the evidence: what the two verify loops are, and whether code alignment explains the gap.

**Outcome.** Keep the code without the mask. The mask repeatedly loses on this Zen 3 machine
(about 2.1-2.3% with 2 MiB pages, about 1.3% with 1 GiB pages), and aligning every loop to 64
bytes did not remove the regression. The 8600G (Zen 4, the 512-bit kernels) shows no difference
between any of the builds. The mechanism inside Zen 3 is not identified.

## Builds

All release builds with the crate's profile (fat LTO, `x86-64-v3`). The `.text` hashes are the
first 16 hex digits of its SHA-256.

| Build | Source | `.text` |
|---|---|---|
| r3 | TODO 97 before the seed mask | `fd754435c435cf47` |
| r4 | r3 + the seed mask | `07da9bbbfa076f91` |
| r5 | r4 + the 24-bit SIMD encoder; rebuilt from the final tree with the mask put back, `.text` identical to the binary measured | `670468669d287d6c` |
| r6 | the final tree, no mask | `ac620c7e8cd31991` |
| r5a | r5's source, `cargo rustc --release --bin tmr -- -C llvm-args=-align-loops=64` | `c3c0c771c9bc6960` |
| r6a | r6's source, the same flag | `3bc61ca3ce170104` |

In r5a and r6a every verify loop of the comparison below is byte-for-byte the loop of r5 or r6
(found by searching `.text`), and starts on a 64-byte boundary.

## The loops (`loops/`)

The 256-bit native verify's sequential main loop, in `simple_native_256`: two lines an iteration,
four ymm loads, four accumulators. Listings with addresses and bytes, and side-by-side diffs.

| Width | r5 (mask): instructions, bytes, 64 B windows | r6 (no mask) |
|---|---|---|
| 16 | 76, 309, 6 | 95, 358, 7 |
| 32 | 76, 309, 6 | 92, 347, 7 |
| 64 | 83, 314, 6 | 82, 318, 5 |

- **The same loads:** order, addressing, unrolling, accumulators and loop control are identical.
- **The masked loop is the simpler code:** a shorter spread, every mask a 32-bit immediate, no
  spill, no reload, no constant rebuilt inside the loop. Its chain from line id to expected value
  is shorter.
- **The unmasked loop has the blemishes:** at 16 bits it rebuilds the hash multiplier and reloads
  the loop bound from the stack every iteration; at 32 bits it has two 64-bit constant loads and a
  six-move register rotation.
- **Modelled work** (Zen 3 tables, no hardware counters): about 6.2 cycles a line masked, 7.8
  unmasked, out of about 48 a line at the measured speed. An estimate: computation, fetch and
  outstanding loads overlap, so it doesn't by itself rule out a CPU-side effect.

## Runs (`runs.csv`, made by `make_runs_csv.py`)

`test=Bench-*-Switch,Bench-*-TMR-4 cycles=1 beat=<width> memory=50% cputype=cores cpus=100%
skip-cores=0`. The Bench pairs pin their epoch, so every build writes the same bytes at a width.

| Set | What | Machine state |
|---|---|---|
| ab4 | r3 vs r4, ABBA, 16/24/32/64 bits | 5700X 1.5 h after boot, 2 MiB pages |
| ab5 | r3 vs r5, ABBA twice at 16/32, once at 24 | as ab4 |
| r6 | the final build, one run at 16 and at 32 | as ab4 |
| ab7 | the 2x2, 4 counterbalanced rounds at each width | minutes after boot, 1 GiB pages; the 5700X noisy (2-6% within a build; post-boot work, a vendor service using CPU) |
| ab8 | the 2x2 on the 5700X, 16 and 64 bits interleaved, 4 rounds of all 8 pairs | 31 min after boot, 1 GiB pages, 0 failed checks |

## Results (Bench-Verify-Switch on the 5700X)

ab8, mean MiB/s of 4 runs (steady-state figures from the 2-second progress lines agree):

| Build | 16 bits | 64 bits |
|---|---|---|
| r5, mask | 46,524 | 46,577 |
| r5a, mask, aligned | 46,437 | 46,579 |
| r6, no mask | 47,028 | 46,503 |
| r6a, no mask, aligned | 47,145 | 46,571 |

- **The mask at 16 bits:** about 1.3% slower (1.27-1.35% by method), the same for both
  alignments. On steady state all 8 masked runs sit below all 8 unmasked ones (exact permutation
  p about 1.6e-4). Two independent rechecks of the raw logs agree.
- **64-byte loop alignment changed nothing** at either width. So it isn't the cure, and the
  six-window correlation was not the cause. Placement effects that this alignment can't move
  (op-cache fit, aliasing with other code) are not excluded.
- **64 bits:** all four builds equal within about 0.1%. That level is the masked 16-bit level;
  only the unmasked 16-bit loop reaches the higher one, about 1.25-1.3% above. ab4/ab5 (2 MiB
  pages) had the same structure, with levels about 45,490 and 46,560 MiB/s (2.3%).
- **Page size isn't isolated:** the gap was smaller in the runs with 1 GiB pages, but the reboot
  and the machine's state changed with it.
- **Controls:** Bench-Init-Switch is equal in every build. Mode 14's verify is about 0.2% slower
  with the mask at 16 bits (it used the same masked helper) and equal at 64. Mode 14's verify (one
  line an iteration) ran about 0.7% faster than the native 64-bit verify: that fits a loop-shape
  hypothesis without establishing it.
- **8600G:** in ab7 no build differs beyond its run-to-run noise (1-4%) at 16, 32 or 64 bits.

## Not established

- Which property of the instruction stream sets the level. Instruction count runs the wrong way
  (95 fast, 76 slow), and the core model leaves the loops far from any core limit.
- Whether it holds on other Zen 3 parts, other boots and other memory placements: one machine,
  two boots, one page size each.
- Detection: identical bytes and checks give identical logical coverage. A different read rate
  changes bus timing, which could change which marginal faults appear, for either build.

## A later experiment

When another change already needs a benchmark on the 5700X: a native verify that processes one
line an iteration (mode 14's shape), with the same pattern bytes and coverage, against today's
two-line loop at 64 bits (DDR4's default width). At 256 bits one line is two vectors, so one line
an iteration feeds only two of the four accumulators: keeping four independent ones means rotating
them over two iterations, which can turn back into today's loop, or using two, which departs from
the four-accumulator rule (measured for latency-bound loops) and needs a cache-resident check.
Today every width fills all four: a pass is 1 line at 128 bits, 2 at 256 and 4 at 512. The
four-accumulator gain was measured on the Intel VM's 256-bit path (about 25% there, N=2 not
enough) and was zero on Zen 5 and at 128 and 512 bits, so two would put Intel at risk, not AMD.
A cheap first probe needs no new code: the 128-bit kernel already checks one line a pass with all
four accumulators, so `Mem-SimpleV2-128` against `Mem-SimpleV2-256` at 64 bits on the 5700X, with
a verify-heavy repetition, tests the shape (its loads are half as wide, a confound).
