# Reproducible TMR pattern audit

The findings and recommendations are in
[the audit report](../../doc/pattern_audit_2026-10-09.md).
The [2026-10-10 follow-up](../../doc/pattern_followup_2026-10-10.md) covers intentional grouped
verification, repeated reloads, signal histories, transformation compositions, and cache exposure.

Audited production commit: `10068d2747ed975c1ee90a13d2e48ec566e4cea6`.
No production source edits. The earlier `doc/pattern_research.md` is a separate, preserved input.

## Files

| File | Purpose |
|---|---|
| `run_repros.py` | Copies source into `target/pattern-audit-source`, appends audit modules there, runs only the added tests |
| `runner_repros.rs` | Cross-width, epoch, seal, parameter, JSON, halt, and dependency reproductions |
| `kernel_repros.rs` | Error-count/interval and mode-2 degeneration reproductions |
| `repro-results.txt` | Captured output: 13 current-behavior reproductions passed |
| `simulate.py` | Independent logical-data model, collision witness, finite-bound counterexample, candidate encoders/decoders |
| `simulation-results.json` | Full deterministic scores and mathematical witnesses |
| `cpu_probe.rs` | Bounded single-thread Rust generator/verification cost experiment |
| `validate_cpu_probe.py` | Independent Python/Rust comparison of complete candidate lines |
| `cpu-results.txt` | Five-round median CPU results and verification timing ranges |
| `inspect_codegen.py` | Extracts function summaries from the latest emitted TMR assembly and the probe assembly |
| `codegen-results.json` | Inspection index, instruction counts, call targets; not a substitute for reading hot loops |
| `followup.py` | Bounded history/quiet-victim metrics and illustrative controller transformations; reuses `simulate.py` |
| `followup-results.json` | Deterministic per-bit history counts, transformation scores, and algebra checks |
| `reload_probe.rs` | Compile-only demonstration: ordinary repeated reads can merge; volatile reads remain |

The reproductions assert **observed current behavior, including defects**. Convert the relevant
assertions when implementing fixes; they are not tests claiming the production code is correct.

## Reproduce

From the TMR-APP repository root, with its existing Rust toolchain and dependencies:

```powershell
python research/pattern_audit/run_repros.py
python research/pattern_audit/simulate.py

New-Item -ItemType Directory -Force -Path target/pattern-audit-probe | Out-Null
rustc -O -D warnings -C target-cpu=x86-64-v3 --edition=2024 --emit=link,asm --out-dir target/pattern-audit-probe research/pattern_audit/cpu_probe.rs
python research/pattern_audit/validate_cpu_probe.py
& target/pattern-audit-probe/cpu_probe.exe | Tee-Object research/pattern_audit/cpu-results.txt

cargo clippy --all-targets
cargo rustc --release --bin tmr -- --emit asm
python research/pattern_audit/inspect_codegen.py
```

To reproduce the follow-up independently of the larger audit:

```powershell
python research/pattern_audit/followup.py
New-Item -ItemType Directory -Force -Path target/pattern-audit-probe | Out-Null
rustc -O -D warnings -C target-cpu=x86-64-v3 --edition=2024 --crate-type lib --emit asm -o target/pattern-audit-probe/reload_probe.s research/pattern_audit/reload_probe.rs
Get-Content -LiteralPath target/pattern-audit-probe/reload_probe.s
```

The emitted `plain_four_reads` contains one memory read; `volatile_four_reads` contains four.
The probe is compiled only, not executed. The follow-up simulation scores 1,024 synthetic
64-byte lines per pattern and never touches TMR's test allocation.

The research scripts do not launch TMR or use its real allocation backend. The extra Rust tests
use small aligned heap regions with guards; the CPU probe uses at most 32 MiB and one thread.
Run timing experiments separately from compilation and simulations.

The baseline production verification was `cargo test --lib` (142 passed) and
`cargo clippy --all-targets` (zero Clippy warnings). The local environment emits a separate
`could not canonicalize path C:\Users\Administrator` startup warning; it did not prevent these
commands or the compiler from completing.

The source copy including the added audit modules also passed:

```powershell
cargo clippy --offline --manifest-path target/pattern-audit-source/Cargo.toml --target-dir target --all-targets -- -D warnings
```

## Interpretation

- Simulation widths are bit lags within a logical 64-byte line.
- DDR4/DDR5 labels assume conventional 64/32-bit payload beats, respectively.
- Scores do not include scrambling, encryption, ECC/metadata pins, idle transitions, or
  command/address traffic.
- DBI simulation is explicitly the DDR4 per-byte DC rule; it is not applied as a claim about DDR5.
- Samples are 1 MiB, not a proof of all-address uniqueness. The report identifies which
  constructions have an injectivity proof and at what granularity.
- The CPU experiment is not a DRAM or integrated TMR bandwidth benchmark. Its buffers may fit
  the host LLC. Formula benchmarks do not reproduce all existing hand-optimized kernels.
- The simulation uses a fixed synthetic base and parameters. Timings and runtime reproduction
  addresses naturally vary.
- The source-copy runner currently shares the repo's Cargo target directory for cached
  dependencies. It does not alter the original source.
- Follow-up history scores include only windows inside a payload burst. Controller masks are
  illustrative algebraic transformations, not reverse-engineered Intel/AMD algorithms.
- A quiet-bit/toggling-bit pair is a logical relationship; no physical pin or row adjacency is
  inferred. Four preceding bits plus the current bit motivates the five-bit history score,
  but does not model an entire DDR5 receiver.
