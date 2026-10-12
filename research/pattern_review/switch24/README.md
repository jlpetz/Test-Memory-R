# 24-bit encoder investigation

The winning SIMD encoder is **applied to the working tree, uncommitted**.
`switch24-simd.patch` is an export of this change alone; do not apply it again.
The [report](../../../doc/switch24_simd_research_2026-10-11.md) explains the results,
repetition tradeoffs and physical-model limits.

Update, 2026-10-12: the encoder remains installed after Opus's reported desktop
measurements. The separate seed-mask change has been reverted, and this encoder's
module declaration has moved to `src/lib.rs`. The archived patch, source hashes,
assembly and test counts describe the October 11 snapshot. The staging script
intentionally refuses subsequent unrelated edits; its reproduction commands below
require that historical snapshot, not the current production tree.

The experiment uses ordinary aligned heap buffers of 8 KiB and 256 KiB. It does
not launch TMR, reserve large pages, exhaust memory or change hardware settings.

From the repository root:

```powershell
python -B research/pattern_review/switch24/repetition.py
cargo clippy --manifest-path research/pattern_review/switch24/Cargo.toml --target-dir target/switch24-probe --all-targets --offline -- -D warnings
cargo rustc --manifest-path research/pattern_review/switch24/Cargo.toml --target-dir target/switch24-probe --release --offline --lib -- --emit asm
cargo test --manifest-path research/pattern_review/switch24/Cargo.toml --target-dir target/switch24-probe --release --offline -- --test-threads=1
```

Read the emitted assembly before running the explicit CPU sample:

```powershell
cargo test --manifest-path research/pattern_review/switch24/Cargo.toml --target-dir target/switch24-probe --release --offline checks::review_switch24_cpu_sample -- --exact --ignored --nocapture --test-threads=1 > research/pattern_review/switch24/cpu-samples.txt
python -B research/pattern_review/switch24/analyze_samples.py
```

The checked-in summary has nine samples for every combination of five candidates,
three available SIMD widths, two buffer sizes and fill/verify. The timing probe
skips AVX-512 on a CPU without it; `analyze_samples.py` expects all three widths,
as recorded on the current box, and will reject a partial dataset.

Candidates:

- `shared`: the original shared scalar packer, including the earlier hash mask.
- `repeat`: construct a 24-bit repetition using 64-bit arithmetic.
- `motif`: construct a 48-bit seed/complement motif using 64-bit arithmetic.
- `shuffle`: scalar byte expansion followed by SIMD placement.
- `lookup`: SIMD byte expansion with a 16-byte register table and SIMD placement.

The isolated full application copy can be reproduced without editing the product:

```powershell
python -B research/pattern_review/switch24/stage_candidate.py --review-probes
cargo clippy --manifest-path target/switch24-candidate/Cargo.toml --target-dir target/switch24-full --all-targets --offline -- -D warnings
cargo test --manifest-path target/switch24-candidate/Cargo.toml --target-dir target/switch24-full --all-targets --offline -- --test-threads=1
cargo rustc --manifest-path target/switch24-candidate/Cargo.toml --target-dir target/switch24-full --release --offline --bin tmr -- --emit asm
```

`stage_candidate.py` restores the three archived input files in that copy before
applying the encoder change. It accepts either the original or installed version
of those files and refuses unrelated subsequent edits. The original files and
hashes remain evidence of this review, not a general patching tool.

| Artifact | Purpose |
|---|---|
| `inputs.json`, `*.baseline` | Original source hashes and the three modified input files |
| `expected-output-hashes.json` | Hashes of the four installed source files |
| `candidate_switch24.rs` | Standalone copy of the winning product module |
| `switch24-simd.patch` | Small, already-applied source change |
| `repetition-results.json` | Independent payload-model results |
| `probe.rs`, `probe-codegen.json` | Candidate kernels and assembly inspection |
| `cpu-samples.txt`, `cpu-summary.json` | Raw measurements and median/min/max |
| `full-codegen.json`, `verify-loops.json` | Complete candidate release loops |
| `product-codegen.json` | Final working-tree release inspection |
| `full-test-results.txt`, `product-test-results.txt` | 192 candidate checks and 179 product tests |
| `validation.json` | Final source, Git and verification status |

Instruction counts are not cycles. Cache-sized CPU measurements are not DRAM
throughput or DDR6 hardware validation. The physical 12/24-bit mappings remain
the same explicitly labeled models as before.
