# Follow-up validation of the native-pattern fixes

The findings are in
[pattern_followup_review_2026-10-11.md](../../../doc/pattern_followup_review_2026-10-11.md).
This round validates the fixes to the original review and the new 12/24-bit models.

**Submitted implementation**

From the repository root:

```powershell
python -B research/pattern_review/followup/run_probes.py
```

This copies the current source into `target/pattern-review-source`, appends only review
modules, and uses `target/pattern-review-artifacts` for Cargo output. It does not change
production files or run TMR. `probe-results.txt` records the original thirteen passing probes:
32,076 injected-corruption checks, 4,352 balanced model seeds and 12,288 legacy-image comparisons.
The interval reproduction remains a documented internal limitation, not a failed review fix.

`review-inputs.json` records the submitted source/spec hashes. `source-delta.patch` records
the incremental changes from the source copy retained by the first review.

**Historical performance candidate — recommendation withdrawn**

The `switch-seed-mask-candidate.patch` preserves all images while limiting byte expansion
to the hash bits used by each profile. It was initially tested in a source copy, then adopted
and subsequently reverted. On October 12, Opus reported a repeatable approximately 2.1%
regression on the 5700X's 16/32-bit verification paths. The corrected recommendation and
reported ranges are in the report linked above. Retain these artifacts as historical evidence;
do not restore the mask on the strength of instruction counts or this local CPU probe.

For historical reproduction on the original reviewed production snapshot, the following applies it
**only to the copy** and repeats its checks:

```powershell
git apply --directory=target/pattern-review-source --check research/pattern_review/followup/switch-seed-mask-candidate.patch
git apply --directory=target/pattern-review-source research/pattern_review/followup/switch-seed-mask-candidate.patch
cargo clippy --all-targets --offline --manifest-path target/pattern-review-source/Cargo.toml --target-dir target/pattern-review-artifacts -- -D warnings
cargo test --all-targets --offline --manifest-path target/pattern-review-source/Cargo.toml --target-dir target/pattern-review-artifacts -- --test-threads=1
cargo rustc --offline --manifest-path target/pattern-review-source/Cargo.toml --target-dir target/pattern-review-artifacts --release --bin tmr -- --emit asm
```

Do not share a Cargo target directory between the source copy and the production crate.
The source copy at the end of this review contained the candidate. Running `run_probes.py` again replaces it
with a fresh copy of the then-current production source.

| Artifact | Evidence |
|---|---|
| `codegen-review.json`, `store-loops.json` | Submitted post-LTO native functions and store-loop candidates |
| `switch-mask-codegen.json`, `switch-loop-comparison.json` | Rejected seed-mask candidate's full-build assembly and matched profile loops |
| `switch-mask-test-results.txt` | 190 passing tests on the candidate |
| `switch-mask-probe-results.txt` | Final thirteen-probe rerun after lint-only probe cleanup |
| `switch16_probe.rs` | Isolated old/current/cast/masked 16-bit CPU experiment |
| `switch16-cpu-*` | Initial three-variant CPU sample |
| `switch16-mask-cpu-*` | Final four-variant CPU sample, including the now-reverted mask |
| `switch16-candidate.patch`, `switch16-cast-codegen.json` | Rejected cast-only experiment: no improvement in the complete release loop |

The isolated probe uses ordinary 8 KiB/256 KiB heap buffers and four OR accumulators. Its
timings describe CPU cost here, not LPDDR bandwidth or TMR performance on another machine.
It also verifies 100,000 matching hash inputs and checks injected corruption.

```powershell
rustc --edition=2024 -O -C target-cpu=x86-64-v3 -C codegen-units=1 --test --emit=link,asm -o target/pattern-review-artifacts/switch16_probe.exe research/pattern_review/followup/switch16_probe.rs
.\target\pattern-review-artifacts\switch16_probe.exe checks::review_perf16_expressions_preserve_images_and_checks --exact --nocapture
.\target\pattern-review-artifacts\switch16_probe.exe checks::review_perf16_bounded_cpu_sample --exact --ignored --nocapture --test-threads=1
```

Compile-only imports of the full `pattern_gen.rs` produce unused-item warnings in this
standalone experiment; the product and candidate all-target Clippy checks are clean.
`inspect_loops.py ASSEMBLY OUTPUT_JSON` extracts store-loop candidates for manual inspection.
Counts include moves and loop control; they are not cycle counts.
