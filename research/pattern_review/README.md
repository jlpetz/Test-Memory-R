# Independent pattern review probes

**The fixes have since been reviewed.** Use the [follow-up probes](followup/README.md) for the
updated APIs and results. The original probes below preserve the first review's defect
reproductions and input snapshot.

Review of the uncommitted native-pattern implementation above commit `468f23e`, 2026-10-11.
Production files are unchanged. The findings are in
[the implementation review](../../doc/pattern_implementation_review_2026-10-11.md), with
[future transfer-profile research](../../doc/transfer_profile_research_2026-10-11.md).

From the repository root:

```powershell
python research/pattern_review/run_probes.py
python research/pattern_review/transfer_profiles.py
python research/pattern_review/review_inputs.py
```

`run_probes.py` copies the current crate's source into `target/pattern-review-source`, appends
only the review test modules, and runs them serially with a separate
`target/pattern-review-artifacts` output directory. It uses ordinary small aligned test
buffers; it does not run the TMR application, allocate live large pages, or stress a remote
machine. The source and artifact directories are disposable Cargo outputs.

**A passing `review_repro_*` test means the defect is present.** These probes assert today's
observed behavior so that findings are reproducible. After a production fix, convert the
corresponding reproduction into a regression test asserting the desired behavior.

| File | Purpose |
|---|---|
| `results_probes.rs` | Pre-step seal halt finalization, plus a prior-completed-step control |
| `pattern_probes.rs` | Reachable linepair collision, odd-round polarity omission, native/legacy switch encoder comparison |
| `kernel_probes.rs` | 24,948 corruption checks, transient-alarm verdict, rescan-off behavior, halt preservation, ignored interval, incomplete persistent-error log |
| `runner_probes.rs` | Native dependent Bench identity mismatch; internal API only |
| `probe-results.txt` | Captured run: twelve passing probes, including six defect reproductions |
| `review-inputs.json` | Production source/config/spec SHA-256 hashes and initial Git state |
| `codegen-review.json` | Post-LTO function summaries and assembly SHA-256; aids manual review |
| `transfer_profiles.py` | Explicit bit placement, unknown-metadata bounds and a generic table-based packing reference |
| `transfer-profile-results.json` | Deterministic simulation output, including model/status labels |

`review_inputs.py` refuses to silently refresh the input snapshot when reviewed source
hashes change. It needs an existing release `tmr.s`; build it with
`cargo rustc --release --bin tmr -- --emit asm`. Its instruction counts do not replace reading
the loops.

The original suite was also checked separately:

```powershell
cargo test --all-targets --offline --target-dir target/pattern-review-baseline
```

This passed 169 tests. Keep copied crates' artifact directories separate: sharing an output
directory allowed Cargo to reuse the copied crate's test executable during an intermediate
check. The original-suite result above was repeated in a fresh directory to remove that
ambiguity. The normal debug test artifact was then rebuilt from the production source and
confirmed to list the original 169 tests; subsequent review probes use their own directory.

The transfer simulation is a **mathematical reference**, not a DDR6/LPDDR6 controller model
or measured bus traffic. Its 12-pin example uses published patent overhead positions with
synthetic payload numbering; its 24-pin example is an abstract extension. Neither is a
verified normative packet map. See the research document for sources and limits.
