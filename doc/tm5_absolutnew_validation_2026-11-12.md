# Check_absolutnew.cfg import validation

Source review, 2026-11-12, TMR HEAD `d6b3279`, plus a live run of this exact file (release build, `memory=10% cputype=cores
cpus=100% skip-cores=0`; stopped by the host's own background-task memory guard at cycle 2/3, test 27/31, 1:34:17 runtime, 0 errors
on every step run — not a TMR crash).

**Implemented in `8675f6c`, reviewed 2026-11-12:** case-insensitive INI section/key matching; `Cores=N` preserved as an explicit worker
count (`CpuConfig.count`); every TM5 import clears the responsiveness reserve (`skip_spec="0"`, not just for `Cores=0`: TM5 never
reserves a core, for any count); TM5's own per-slot defaults for an omitted test field (function, parameter, pattern mode/params,
block size; `Enable` defaults to on). The 2026-11-12 follow-up now also supplies the default enabled Test0 when its section is
absent. Missing whole ordinary test sections remain a compatibility gap.
The follow-up also reverts `8675f6c`'s block-code-0 floor at the owner's request; its overlap-work effect is described below.
Interleave Type and DLL Name remain outside the approved importer work. Native patterns stay as designed;
no parameter-specific mode-2 revival. The owner declined a complete-file import regression fixture on 2026-11-12; existing
focused regression tests remain.

Input: `../TM5_Source/Check_absolutnew.cfg`, named `ABSOLUT(01102021)` by anta777.
Reference: `../TM5/` assembly, cross-checked with `../TM5_Source/MT0.cxx`.

## Verdict

The current importer preserves this file's sequence, repetition counts, block-size ladder, SimpleTest stride, and mirror modes.
The native implementation is a modernized workload with that structure, not a reproduction of every TM5 data background or traffic
condition. Source inspection supports correct coverage and non-cancelling verification on the native SimpleTest paths inspected.
It cannot establish equal error sensitivity, DRAM traffic, speed, or the instructions emitted by the current compiler.

Test 1's two nonzero pattern parameters are deliberately replaced by the native program. This is an accepted pattern redesign, not an
importer defect or evidence that error detection regressed. It limits claims of identical data-background coverage. There are also
confirmed importer defects for other valid or minimally edited TM5 configs, detailed below.

## Exact execution map

For SimpleTest and both mirror tests, both implementations calculate:

`N = max(1, floor(test_time_percent * global_time_percent / 2000))`

Here global time is 1250%. SimpleTest performs four rounds per chunk, each one write followed by N complete verification sweeps.
The mirrors perform N complete round trips before one seal verification.

`W` below is the smaller of 1536 MiB and the thread's available test extent in TMR. An explicit block is also capped by that extent.
All imported tests cover their thread's full allocation by visiting chunks; a 4 MiB block does not mean only 4 MiB is tested.

| TM5 ID | Occurrences per suite cycle | Chunk | N | TMR operation |
|---|---:|---:|---:|---|
| 0 | Implicit seal; absent from explicit sequence | W | Ignored | Seal before/after correctness work and at cycle boundaries |
| 1 | 1 | 4 MiB | 150 | Sequential SimpleTest, mode 2 converted to lines program |
| 2 | 6 | W | 5 | Sequential SimpleTest, mode 1 converted to linepair program |
| 3 | 2 | W | 12 | MirrorMove128: jump 2; 128 B units, 384 B period |
| 4 | 3 | W | 12 | MirrorMove: four subblocks, traversed in lockstep |
| 5 | 3 | W | 12 | MirrorMove128: jump 1; 128 B units, 256 B period |
| 6 | 1 | 4 MiB | 150 | Sequential SimpleTest, mode 0 program |
| 7 | 1 | 8 MiB | 75 | Sequential SimpleTest, mode 0 program |
| 8 | 1 | 16 MiB | 37 | Sequential SimpleTest, mode 0 program |
| 9 | 1 | 32 MiB | 18 | Sequential SimpleTest, mode 0 program |
| 10 | 1 | 64 MiB | 10 | Sequential SimpleTest, mode 0 program |
| 11 | 1 | 128 MiB | 5 | Sequential SimpleTest, mode 0 program |
| 12 | 1 | 256 MiB | 5 | Sequential SimpleTest, mode 0 program |
| 13 | 0 | 512 MiB, declared only | 5 | Correctly omitted: absent from Test Sequence despite Enable=1 |
| 14 | 1 | W | 5 | Sequential SimpleTest, mode 0 program |
| 15 | 8 | W | 5 | Strided SimpleTest, mode 0 program |

There are 31 explicit steps per cycle: 23 SimpleTest steps and eight mirror steps, over three suite cycles (93 explicit steps).
Repeated IDs remain repeated steps; they are not deduplicated. The seal is additional work.

**Test 0 need not appear in the sequence.** ABSOLUT declares `[Test0]` with `Enable=1` and `Function=RefreshStable`, but omits
0 from `Test Sequence`. TMR correctly enables the run's seal independently of that sequence. Seal checks remain part of the
harness and cycle boundaries, with errors recorded separately as seal errors. Listing 0 would add an explicit `Seal-Check` step.
The missing-section defect described below concerned deleting the entire `[Test0]` section and is now fixed.

Test 15's `Channels=2, Parameter=256` means a 511-line skip after each 64 B access, hence a **512-line / 32 KiB period**.
The passes start at offsets 511 down to 0 and each walks that period to the chunk's end. Both write and verify visit every line.
511 is the skip, not the distance from one accessed line to the next.

Sources:

- `src/config.rs:1550` parsing; `:1653` conversion; `:1816` block sizing; `:1873` mirrors; `:1893` repetitions.
- `src/config.rs:1014` cycle plan; `src/runner.rs:4553` registration and expansion into steps.
- `src/test_harness.rs:180` write/read repetition loop.
- `src/tests.rs:4005` stride period; `:4029` actual stride walk; `:4576` subblocks; `:4595` mirror jump walk.
- TM5 `mt_ini.asm:287`, `MainThread.asm:566`, `bin/mtests0.asm:237`, `:298`, `:1135`, `:1523`, `:1566`.
- Decompiled cross-check: `MT0.cxx:664`, `:684`, `:906`, `:1129`.

## Findings that warranted importer fixes (built 2026-11-12)

### Fixed: Case-sensitive INI keys can silently remove stress

Before the fix, TMR stored sections and keys in case-sensitive HashMaps and looked up exact strings. TM5 uses Windows
`GetPrivateProfileString`, whose section/key matching is case-insensitive.

Concrete change to this file: spell Test 15's key `parameter=256`. TM5 still reads its stride. Before the fix, TMR did not find
`Parameter`, defaulted it to zero, and ran the test sequentially. The malformed-number check never saw the value.

Fixed: section and key names are matched case-insensitively (`.to_ascii_lowercase()` at both insertion and lookup); stored values,
including function names, keep their case, matching `GetPrivateProfileString`'s own asymmetry.

Sources: `src/config.rs:1574-1601`, `:1638-1642`. The original ABSOLUT file uses the expected capitalization, so it was not affected.

### Fixed for present sections: Missing fields did not use TM5's defaults

Before the fix, TMR defaulted an omitted `Enable` to zero. TM5 initializes each test's enable flag from `TableEnableTst`, whose
entries are all one. Deleting `Enable=1` from Test 15 left the TM5 test enabled but caused TMR to skip all eight occurrences.

Other missing values also differ: TM5 has per-test defaults for Function, Parameter, Pattern Mode, pattern parameters and block size.
For example, TM5's default Parameter for Test 2 is 254; TMR previously defaulted it to zero. Missing Function became `Unknown`
rather than the indexed TM5 default. Missing whole ordinary test sections are still not synthesized as they are by TM5's loading loop;
the special Test0 seal now is.

Fixed: TM5's own 16-entry-per-slot default tables (`TM5/bin/DefTestParam.inc`: function, Parameter/Jump, Pattern Mode, Pattern
Param0/1, Test Block Size) are ported directly as `TM5_DEFAULT_*` consts and applied when a present `[TestN]` section omits that
field; `Enable`'s own default flips from off to on (`TableEnableTst` is all 1).
Cross-checked against the TM5 author's own notes (`TM5_Author_website_notes.txt`): the default Test1 `Pattern Param0=0x1E5F` is
exactly the table's 7775 decimal.

The follow-up fixes missing Test0 by supplying an empty section to the existing per-slot defaults. With `Test Sequence=1,2`
and valid sections for Tests 1 and 2, these cases now all match TM5:

| Test 0 declaration | Current TMR import | TM5 behaviour |
|---|---|---|
| `[Test0]`, `Enable=1`, `Function=RefreshStable` | Seal enabled despite 0 being absent from the sequence | Seal enabled |
| `[Test0]` present, Enable and Function omitted | Seal enabled using the new per-field defaults | Seal enabled |
| `[Test0]`, `Enable=0` | Seal disabled | Seal disabled |
| Entire `[Test0]` section absent | Default enabled RefreshStable slot supplies the seal | Default enabled RefreshStable slot supplies the seal |

Previously the last row disabled sealing, and explicitly listing 0 without its section was rejected. Both are fixed. An explicit
`Enable=0` is preserved and command-line `seal=` overrides still win. The regression test covers absent, empty, enabled and disabled
Test0 sections, with 0 absent from the sequence, explicitly listed, and with no explicit sequence.

Checked all 11 `.cfg` files in `../TM5_configs/bin`: each declares Test0 as enabled RefreshStable. None has the missing-section
defect. ABSOLUT, Extreme1, Ollie and SuperLight2 omit 0 from their sequences; their implicit seals already worked.

Known compatibility difference: entirely absent slots 1-15 are left out, and a sequence reference to one is rejected. TM5 loads
its ordinary default slots according to `Tests=`: sections override built-in definitions rather than create tests from scratch.
For example, `Tests=4`, `Test Sequence=1,2,3` and no `[Test2]` uses default Test2 in TM5; TMR reports an undefined test.
This does not justify adding unrequested tests to a sequence. No supplied config relies on missing ordinary sections, and
implementing that default-loading behaviour is outside the completed importer batch.

Sources: `src/config.rs` (the `TM5_DEFAULT_*` consts and the test-parsing loop); tests `omitted_test_fields_take_tm5s_own_per_slot_defaults`
and `tm5_test0_defaults_preserve_the_seal_and_explicit_disable`.

### Fixed: A nonzero Cores value loses its count

Fixed two ways together: `CpuConfig.count: Option<u32>` carries `Cores=N` through as an explicit worker count (not folded into
`cpu_type`), reusing the existing `cpus=`-style clamp-to-available and over-request warning; and every TM5 import clears TMR's own
responsiveness reserve (`cpu_pinning.skip_spec = "0"`), since TM5 itself never reserves a core, for `Cores=0` ("ALL") or any
positive count. Without the second half, `Cores=0` on an 8-core/16-thread box would still leave 14, not 16, and a positive count at
or above the machine's full thread count would be capped one short of it for no TM5 reason — confirmed by a regression test against
a synthetic 8-core/SMT topology (`Cores=0`→16, `100`→16 capped, 1/4/9 unaffected either way since they're below the reserve's
effect). `cpu_selection::choose` already picks whole distinct physical cores before reaching for SMT siblings, so no `cputype=`
switch was needed for small counts.

Sources: `src/config.rs` (`CpuConfig.count`, the `cpu_pinning` literal in `to_modern_config`), `src/cpu_selection.rs` (`choose`,
unchanged); test `tm5_cores_count_reaches_cpu_selection`. Confirmed live: the dry-run plan for this file went from "100% of 6
available" (skip-cores=1 default) to "100% of 8 available", using all 8 logical CPUs on the dev box with no "Skipping" line.

### Declined: Interleave Type and DLL Name are silently ignored

`DLL Name` is `bin\MT0.dll` in the 163 sampled entries; TMR implements the supported functions directly. `Interleave Type=1`
appears in all 11 sampled configs. The owner deferred additional importer handling for these fields.

`Interleave Type` affects TM5's access grouping and pass decomposition. In the reviewed two-channel case, the alternatives make
64 B or 128 B visits at the same period. Modern CPUs can execute either software access shape. The earlier explanation that
ganged controllers required an external northbridge was wrong: AMD Family 10h integrated controllers supported ganged and
unganged operation ([AMD BKDG 31116](https://www.amd.com/content/dam/amd/en/documents/archived-tech-docs/programmer-references/31116.pdf),
section 2.8). Neither the historical mode name nor channel count supplies a modern controller's full address mapping.

The separate [access-geometry proposal](access_geometry_proposal_2026-11-12.md) evaluates grouped visits as native stress
variation. It does not require reviving the legacy importer option.

## Pattern intent: preserved properties and lost specificity

The import uses these programs:

| TM5 mode | Native program |
|---|---|
| 0 | ident, switch, random, switch |
| 1 | linepair, switch, random, switch |
| 2 | lines, switch, random, switch |

Each family consumes **two write rounds**, P then its complement. Four rounds therefore execute two families per suite cycle:

| Suite cycle | Rounds 1 and 2 | Rounds 3 and 4 |
|---|---|---|
| 1 | Mode's intent family, P then ~P | switch, P then ~P |
| 2 | random, P then ~P | switch, P then ~P |
| 3 | Mode's intent family, new key, P then ~P | switch, new key, P then ~P |

This is documented, intentional scheduling, not a loop bug. The key includes the run seed, suite cycle, step position and pair.
Each repeated Test 2 or Test 15 occurrence receives its own step key. It does mean:

- A run stopped during cycle 1 has not yet run the random family in these imports.
- In cycle 2, Tests 1 and 6 have identical shapes and family schedules, with different step keys; Test 2 has no linepair phase.
- For a completed three-cycle run, the original intent family gets four of the twelve write rounds, random two, and switch six.
  This reallocates stress among backgrounds while preserving the total write and verification counts.

**Test 1 loses its parameter-specific background.** TM5 uses 0x77777777 and 0x33333333 in packed arithmetic updating its pattern
registers. TMR's native conversion discards both parameters, and `lines` derives an odd arithmetic step and seed from the line ID
and epoch key. Two otherwise identical mode-2 configs with different parameters convert to the same native program and, given
the same seed and layout, the same data.

This was part of the native-pattern redesign, so it is not an accidental failure to feed a kernel. It is nevertheless a limit on
the claim that the config author's specific stress pattern is preserved. The original generator is also not simply "write
0x77777777 and 0x33333333": those values are arithmetic inputs.

Recommendation after the owner's follow-up: keep the native program. Its independent line seeds preserve position purity across
overlapping chunks, its odd arithmetic step guarantees eight distinct words within a line, and the program adds explicit P/~P phases,
changing keys, switching and random phases. Global word identity comes from the random phases; `lines` does not promise it across
every word of the run. The earlier collapse was in TMR's old mode-2 adaptation, not proof of a flaw in TM5's generator.

The SIMD/memset issue is separate: LLVM can rewrite byte-uniform fills into a memset call and erase the requested SIMD width.
TM5's mode-2 parameters feed packed multiplication/addition and do not simply fill the buffer with 0x77 or 0x33. Avoiding memset is
therefore not, by itself, a reason those parameters must be discarded. The reason to keep today's design is its chosen coverage and
execution properties. This review found no concrete counterexample requiring a parameter-specific pattern to return.

Original mode/parameters could be retained as import provenance without affecting the generated data. Revisit the background only if
a reproducible failure exposes a gap in the native program, or a new requirement specifically calls for that background. Merely
hashing the old parameters into a key would change the bytes without restoring their original structural effects.

`linepair` complements each aligned pair of lines. It does not make every adjacent pair of lines complementary, and it is not the
same packed subtract/invert sequence as TM5 mode 1. It preserves a related neighbouring-line contrast objective.

`patterns=legacy` retains TMR's earlier numbered implementations, not bit-exact TM5. In particular mode 2 uses 64-bit arithmetic
and restarts its chain per 4 KiB page. It is a comparison baseline, not an exact-reproduction solution.

Sources: `src/config.rs:1693-1735`; `src/pattern.rs:161-240`, `:518-554`; `src/runner.rs:1422-1426`;
`src/tests.rs:6604-6638`; TM5 `bin/mtests0.asm:1873-1931`.

## Other stress differences to expose, not silently equate

- **CPU participation:** fixed, see above — ABSOLUT's Cores=0 now imports as literally every logical CPU, not TMR's usual
  one-core-held-back reserve.
- **Thread timing:** TM5's workers can advance independently through RunTestSequency; the source has synchronization around
  allocation and the cycle boundary, not each test. TMR starts each step at a worker barrier and collects every result before the
  next step. This can reduce mixed-test traffic between workers while increasing simultaneous same-test load. It does not
  synchronize every inner write/read round. Sources: TM5 `MainThread.asm:566-761`, `:478`; `src/thread_pool.rs:470-472`,
  `src/runner.rs:1549-1568`.
- **Cache and prefetch:** SimpleTest uses cached stores and loads. TMR imports it without flush-before-verify. Small 4/8/16 MiB chunks
  can spend much of their repeated verification in cache on modern CPUs; TM5's cached accesses do not guarantee DRAM access either.
  TM5 issues PREFETCHNTA; TMR's sequential native loops have no explicit prefetch, while the strided loop uses PREFETCHT1 with up to
  128 steps of lookahead rather than eight. These are measured TMR optimizations, but they change request timing and cache use.
  Preserve the small-block ladder: automatically flushing every pass would change its cache stress. Sources: `src/tests.rs:3972`,
  `:3996`, `:6420`, `:6460`; `src/config.rs:1238`; TM5 `bin/mtests0.asm:290-294`, `:362`, `:453`.
- **SIMD width:** imported tests use Auto, normally the widest available width. MirrorMove128's "128" denotes a 128-byte swap unit,
  not a request for a 128-bit SIMD kernel. The jump tests retain that unit; the four-subblock mirror's vector granularity follows
  the selected SIMD width. This preserves the broad access shape, not identical load/store issue order.
- **Coverage and memory lifetime:** TMR retains its allocation across cycles and spreads equal-size chunks over it, overlapping when
  necessary. TM5 releases and reacquires memory between cycles and works through AWE windows. The TMR walk covers the entire
  extent; it need not duplicate TM5's tail omission or allocation churn. Uneven sizes can add overlap work, and a fixed VA does not
  imply known physical rows. Sources: `src/test_memory.rs:19-80`; TM5 `MainThread.asm:458`, `:737-744`.
- **Block-code rounding reverted:** `8675f6c` made `tm5_block_size` floor code 0 (divisor 1) to lock granularity, matching
  `MainThread.asm:627-660`. That is now reverted at the owner's request. Ollie's 1236 MiB window had become 1232 MiB. Other
  configs also changed when the available extent capped the window to a nonmultiple of granularity: 500 MiB became 496 MiB.
  Because `ChunkSpread` uses complete overlapping chunks, that produced two chunks at offsets 0 and 4 MiB, traversing 992 MiB
  per sweep instead of one 500 MiB chunk. No memory was omitted, but overlap was 492 MiB and traversal work increased 98.4%.
  The earlier description as merely a small alignment difference understated the effect. Code 0 now keeps the smaller of the
  window and extent, subject to TMR's ordinary 4 KiB granules; codes 1-3 retain lock-granularity flooring. Regression tests cover
  Ollie's window and the one-chunk layout when the extent caps the window, including total traversal and overlap.

## Verification and seal assessment

The inspected native write and verify paths calculate the same line image from stable thread/offset IDs and the same phase.
The sequential and strided walks cover whole lines; the verify ORs word-wise XOR differences into accumulators. Multiple bad words
cannot cancel as they could in a plain XOR checksum. A failed fast check enters the rescan path, and a rescan that reproduces
nothing still returns an error. This preserves transient detection.

The mirror tests swap each pair twice in a complete round trip and then check the seal. All 12 round trips execute before that
check, matching TM5's structure. Neither test proves every intermediate move was correct merely because the final image is right;
that limitation is shared with TM5, not introduced by the importer.

Test 0's Time=8 does not prescribe a sleep or five verify passes. TM5's test 0 is the seal service. Mapping it to TMR's seal,
despite its absence from Test Sequence, is correct. TMR's default seal adds full-word address identity and paired-cycle polarity,
and fills with non-temporal stores followed by SFENCE. Its traffic and data differ from TM5's cached motif fill, while retaining
retention-under-load and stray-write checking.

Halt is checked before replacing data after a failed seal check and after each SimpleTest verify. Accumulator grouping is not a
coverage defect and should remain on the fast path.

Sources: `src/tests.rs:6449-6508`, `:6515`, `:3834-3858`, `:4687-4695`; `src/test_harness.rs:171-215`;
`src/seal.rs:243-248`, `:304-318`, `:438-486`; TM5 `MainThread.asm:355-446`, `:671-732`.

The existing source tests cover synthetic conversion cases, sequence repetition, mirror modes, pattern phase scheduling, SIMD images,
guard regions and injected word errors. The owner declined an additional fixture pinning the complete ABSOLUT execution plan;
the existing focused regression tests remain.
Those tests were inspected, not run. The local TM5 assembly and decompiled source also differ in RS_Set: assembly lines 76-79 omit
the third store that MT0.cxx lines 532-535 contain. The sensible complete 48 B seal motif is supported by the decompiled implementation
and the assembly checker; the source snapshots should not be advertised as one reproducibly matched TM5 executable.

## Status

1. Fixed: INI key normalization, defaults for fields of present sections, the default Test0 seal when its whole section is absent,
   the explicit Cores count and the imported reserve.
   **Known compatibility difference:** omitted ordinary sections are not synthesized; an explicit sequence reference is rejected.
   DLL/interleave importer additions remain deferred.
2. **Declined:** the complete-file ABSOLUT regression fixture (owner, 2026-11-12). No further fixture work is planned.
3. **Proposed next improvement:** show original-to-effective pattern substitutions at import and retain their source metadata
   in saved results. The owner supports making this visible; the proposed presentation is below.
4. Decided: keep the native pattern redesign and full verification of each written polarity. Parameter-specific mode-2
   reproduction is not required; revisit only on concrete coverage evidence.
5. Decided: hardware error-sensitivity and fresh assembly validation stay outside this source-review verdict.

## Proposed presentation of imported patterns

TMR already shows the effective program in each step's Config report (`runner::step_config_line`) and records it, the pattern
version, run seed and transfer profile in `run_context`. The missing information is which TM5 pattern it replaced.

Use the existing startup plan/report area, shared by console and window, for an informational note:

> TM5 import: SimpleTest data patterns converted to TMR native programs.

Under it, group tests sharing the same source mode and effective program. For unmodified native ABSOLUT:

| Tests | TM5 mode | TMR program |
|---|---|---|
| 1 | 2 | lines → switch → random → switch |
| 2 | 1 | linepair → switch → random → switch |
| 6–12, 14–15 | 0 | ident → switch → random → switch |

These are programs, not a claim that all four families run in one suite cycle. Each family uses a verified P/~P pair; with
ABSOLUT's four rounds a step consumes two families per cycle. Keep the exact schedule in the existing pattern explanation
rather than implying that a program row describes four individual write rounds.

Produce the note from the effective plan after CLI overrides, and include only tests selected to run. Overrides such as
`pattern-mode=` must show what will actually run. With `patterns=legacy`, label the choice as TMR's legacy adaptation;
never describe it as exact TM5 reproduction. Mirror tests do not consume these pattern-mode fields and should not appear
as converted SimpleTests.

Keep original mode, Param0/Param1 and their replacement status in optional import metadata alongside the already recorded
effective program. Details for Test 1 can say `Param0=0x77777777, Param1=0x33333333: replaced by native program`; these are
provenance, not new generator inputs. Preserve this metadata through config export and result saving, with absent fields
accepted for older files. Share the same conversion information with the detailed log/report so it remains available later.

This needs no new prompt, repeated per-chunk logging, or hot-loop bookkeeping. The default view should communicate the
substitution in a few grouped lines; raw hexadecimal parameters belong in details and saved metadata.
