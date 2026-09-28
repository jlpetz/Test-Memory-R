# TODO 74. [TMR-APP] Test sequencing: an ordered per-cycle plan, with TM5 `Test Sequence` converted into it

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: High. It is the largest TM5-compatibility gap: TMR runs a different plan from every
community `.cfg`.
**Raised**: 2026-09-27. Split out of #71 decision 1 so that the #69/#71 cleanup could commit
without it.
**Status**: the outline design below is agreed. Open points A-C need mockups and decisions first.

**The gap.** `to_modern_config` (`config.rs` ~1181) pushes the enabled tests in file order, once
each. `Test Sequence` goes into `LegacyMetadata.tm5_test_sequence`, and only
`get_test_configs_with_sequence` reads it, which has had no caller in any commit. Every `.cfg` on
disk has all 16 tests enabled, so the sequence alone shapes TM5's plan:
- `1usmus_v3`: TM5 runs 17 steps, starting 6,12,2,10, with test 1 twice.
- `Check_absolutnew`: TM5 runs 31 steps, with test 15 ×8 and test 2 ×7, and never runs 0 or 13.
  TMR runs all 16 once, 0 and 13 included.
- `MT.cfg`: TM5 runs 32 steps.

Order matters beyond coverage, because MirrorMove checks whatever data the *previous* test left.
**Don't wire the latent implementation** (`get_test_configs_with_sequence` /
`get_tm5_sequence_configs`, kept under `#[expect]`). It indexes `test_sequence` by TM5 id, but that
list holds only the enabled tests, so every index shifts once a test is disabled. Its per-test
builder also omits `flush_before_verify`. Delete it when this lands.

**What TM5 does** (`RunTestSequency`, `MainThread.asm:566-770`):
- A step whose test is disabled is skipped, not an error.
- `Time (%)` applies inside each run of a test, so every appearance runs at its full setting.
- Every step is wrapped by test 0 (RefreshStable), on each chunk, in one of two ways (re-read
  from `MainThread.asm:673-728` on 2026-09-28; drawn in `doc/memory_allocation_models.md` §1.5):
  - a normal step: test 0 checks the data it left (errors numbered 0) and refills only if that
    check failed. Then the step's test runs its own check (errors N), then test 0 refills,
    always. A step that is test 0 itself runs only the first check;
  - a MirrorMove or MirrorMove128 step (`Capable_UseTst0ForGenAndCheck`,
    `bin/function.asm:891/952`) has no check before it. MirrorMove runs, then test 0 checks what
    it left (errors numbered **N**, MirrorMove's) and refills only if that check failed. So
    retention damage since the previous refill is blamed on MirrorMove too. MirrorMove never
    verifies its own data, and the step is skipped when test 0 is disabled.
- A cycle (`MainThread.asm` ~340-485): test 0 fills all memory, with no check, then the sequence
  runs with every step wrapped as above. After the last step, test 0 checks all memory once more
  (errors numbered 0). Then the memory is released (~459), and the next cycle starts again with a
  fill. So test 0 is not a step: it runs around every step and once at the end of each cycle.
  Test 0 has two halves, the fill (`Cmd_Set`) and the check (`Cmd_Check`): a cycle opens with a
  fill only and closes with a check only. The final check verifies the last step's refill, which
  no pre-check follows. It only repeats work when the last step is a MirrorMove, whose own verify
  has just run. TMR keeps its memory across cycles, but should still fill at the start of each one
  (the user, 2026-09-28): that also repairs whatever the final check found. TM5's release and
  re-acquire may also have landed on different physical pages each cycle (inferred, unverified).

**Agreed outline (2026-09-27):**
- **JSON:** each test gets an optional unique `id`. A new optional top-level `cycle_order` lists
  ids and may repeat them. Without it, a cycle is the enabled tests in file order, as today.
  `enabled: false` means declared but skipped, even when listed. An unknown id is a load error.
  The same test with different settings is two tests with two ids.
- **Plan display:** one full cycle as it will run, repeats included and skipped steps marked. The
  ticker's `Test i/n` counts steps.
- **Results:** tests are keyed by step and id, not by name. `--compare-results` matches by name
  today, which breaks as soon as a test repeats.
- **TM5 converter:** each `[TestN]` becomes id `"N"` with its `Enable=`, `Test Sequence` becomes
  `cycle_order`, and `Cycles` becomes the global cycle count. The TM5 id rides on `TestConfig`, and
  the JSON and TM5 paths share one per-test builder.

**Open before building:**
- **A) Who validates between steps: not "wrap every step in test 0".** The user's position
  (2026-09-27): TMR is a broader suite than TM5. Correctness tests should always validate memory,
  but a test may own its own validation. Patterns get reused and may differ between tests, and
  bandwidth and latency tests sit in the same sequence. So TMR needs a modern model, the way
  64-bit allocations replaced AWE. TM5's configs must still run close to how TM5 ran them. This
  ties to #13 (PatternState: what pattern a region holds, so a later step can verify it, or knows
  that it can't).
  **The architecture question inside A** (the user, 2026-09-28: decide it when #74 is picked up,
  don't lose it). Should each test keep its verifier inside its own kernel, or should verification
  be separate, so that patterns and verifiers can be reused and swapped? Today every test has its
  own fused, macro-stamped loop per width. That costs duplicated verify code and fixes which
  verifier goes with which pattern. Separating them risks the slowdown that deleted
  `test_framework.rs`. But that cost came from a call boundary on every element: trait and closure
  calls that `#[target_feature]` doesn't cross. A verify *pass* called once per chunk is a single
  call per several MiB and should cost nothing, if the pass is itself a macro-stamped kernel for
  each pattern and width. What a separate pass really gives up is fusion: a test that checks and
  rewrites in one sweep then needs two memory passes. So the unit of reuse is probably the
  *pattern* (a generator macro), stamped both into fused test loops and into standalone fill and
  verify passes, with #13 recording which pattern a region holds. Way to decide: stamp one pattern
  both ways, then compare throughput and asm at each width. TM5 configs need the test-0 wrap either
  way (see the cycle above), because nothing else finds MirrorMove's errors.
  **First thing to try when #74 is picked up** (the user, 2026-09-28): that pilot, on MirrorMove
  and SimpleTest. They are the two ends of it. MirrorMove has no verifier of its own, so it is the
  pure case for a separate pass. SimpleTest writes and reads in one fused loop, so it is where
  losing fusion would show. The user's caution: even one call per chunk was thought too costly,
  which is why the tests were bundled. So measure the call cost too; don't assume it is free.
  **Where the test-0 wrap should live: per chunk, run by the harness** (the user asked 2026-09-28:
  a separate stage between tests, or baked into each test?). First, TM5's final check is not a
  double. It runs once per cycle, one read pass against at least three passes per step. Each refill
  is checked exactly once: by the next step's pre-check or, after the last step, by the final
  check. It repeats a read only when the last step is a MirrorMove, whose own test-0 verify has
  just run, and even then the two reads are a retention window apart. What the wrap buys is that
  test 0's pattern stays a sentinel on every chunk but the one under test. Each chunk's data sits
  for about one step's run while that step works the other chunks, so retention loss and
  disturbance from the step's traffic are both caught. A whole-memory stage between steps can't do
  that: the next step overwrites the data before anything checks it, so the retention window
  shrinks to the gap between two passes. Baking it into each test would stamp check-0 and fill-0
  into every test at every width, the duplication A wants gone. So the harness should wrap each
  chunk, calling standalone check-0/fill-0 kernels (stamped per pattern and width) around the
  test's chunk body. That is the pilot above at chunk granularity, so the pilot measures this
  call cost too. TMR-native sequences could leave the wrap off per step, once #13 says what a
  region holds. The three placements, and why the stage loses the wait (loop order and the
  retention grid: `TMR-APP/doc/memory_allocation_models.md` §1.5):
  ```
  (a) Stage between tests    A (all chunks) → fill0 (all) → check0 (all) → B (all chunks)
      T0's data sits for one pass with nothing else running. It never sees a step's traffic.
  (b) Baked into each test   each test's chunk = [check0 · A · fill0], one combined loop
      Behaves like TM5, but check0/fill0 get copied into every test at every SIMD width.
  (c) Harness per chunk      harness: check0 ─ A's chunk body ─ fill0   (recommended)
      Behaves like TM5. Two shared routines per pattern and width, two calls per chunk.
  ```
  **The user's position (2026-09-28): avoid (a) in most cases**, because it destroys TM5's key
  wait. For TM5-like running keep the wrap, (b) or (c), since that wait is key test functionality.
  But TMR is a modern remake with tests TM5 never had (bandwidth, raw throughput, latency), so the
  wrap may not apply to every test. Make it a per-test property, not something every test gets.
  **The user's idea: skip the unconditional fill-0 in TMR-native mode**, to lengthen the wait.
  What the fill is for: test N has written its own data over the chunk, so the fill puts back the
  one pattern the next pre-check knows. Repair is a side effect (it overwrites test N's errors).
  Skipping it needs #13: the next pre-check would have to verify test N's pattern, so it must know
  which pattern and seed the chunk holds. It saves one write pass per chunk per step, but barely
  lengthens the wait: the chunk's last write moves from the fill back to test N's last write, a
  single chunk-pass earlier in the same slot. A longer wait means leaving a chunk untouched for
  longer, which needs its own knob, e.g. a Mem-Refresh-style delay, or chunks held back as
  sentinels across several steps. Skipping the fill also does not save a DRAM write "because the
  value is the same": x86 doesn't drop same-value stores, and after a normal step the values
  differ anyway.
- **B) Mockups first**, for the plan display and for the end-of-run report. #69 F's per-cycle
  report has to fit in this too. **The user's lean (2026-09-28): one row per step in both**, repeats
  included, so the final report lines up with the plan row for row. When in the sequence an error
  happened matters as well as which test found it. So per-test totals, which community guides key
  on, come from the error log (C) and the TM5 test-number column, not from a second consolidated
  table.
- **C) A sequenced error log.** TM5 listed errors in order, each numbered by the test that found
  it. Specific tests surface specific overclocking weaknesses, so that ordering let users triage.
  **How TM5 numbers them** (read from the asm on 2026-09-28, not run): `SetError`
  (`WindowSupport.asm:1180`) is the only recorder. Each call adds one to a global total, turns the
  test's cell red, and logs `Error in test #N through <elapsed>.` (`log.inc:39`). There is no
  cycle, address, thread or per-test count. The number is passed explicitly, and the attribution is:
  - test 0's check *before* step N reports **0**, not N and not the previous step
    (`MainThread.asm:710-713`). So a `0` means data test 0 wrote after the previous step went bad:
    that step damaged memory outside its own work, or retention failed. TM5 doesn't distinguish;
  - step N's own check reports **N** (`:724-728`);
  - in a MirrorMove/MirrorMove128 step, test 0's verification reports **MirrorMove's number**
    (`:681-693`). MirrorMove never compares data itself, so its errors are really test 0's check;
  - the final check after the whole sequence reports **0** (`:443`).
  So the test-0 wrapping above gives two attributions: a pre-check error is test 0's, a
  MirrorMove verify error is MirrorMove's. A converted TM5 config needs both to produce the same
  numbers. TM5 also logged elapsed time only from CPU0's clock, so worker-thread lines likely read
  `through .` (inferred, unverified).
  Want: each error with timestamp, cycle, step, test and thread/CPU, plus the TM5 test number when
  running a TM5 config, so community guides still apply. This ties to #27 (error isolation) and to
  #29's event stream, where an error is one event and console, log and JSON are renderers of it.
