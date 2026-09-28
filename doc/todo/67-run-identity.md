# TODO 67. Results files must record run identity + config ✓ (2026-09-09)

> Full record, moved verbatim out of `TODO_ARCHIVE.md` on 2026-09-28 (closed item).
> The short entry in `TODO_ARCHIVE.md` holds the current status; this file holds the reasoning.

Comparing a 2026-07-31 baseline against two 2026-09-08 runs produced confident nonsense — StuckBit
"+10-20% faster", Refresh "-14% slower" — with no code change between them. The baseline ran
**8 threads × 7 GiB (56 GiB)** and the new runs **8 × 3 GiB (24 GiB)**: the smaller working set is
more cache-resident (StuckBit up) while Refresh's fixed 64 ms sleeps became a larger share of a
shorter run (Refresh down). **None of it was discoverable from the result files** — it had to be
reconstructed from `logs/`, and `--compare-results` had no basis on which to warn.

**Items 1 + 2 (commit b58db84)** — new `src/run_context.rs`:
- `RunIdentity` replaces the dead `SystemInfoSnapshot`, which was constructed once with hardcoded
  literals (`cpu_brand: "Unknown"`, `total_memory_gib: 0.0`) and had **no setter anywhere**. Detected
  in `TestRunResult::new()` rather than via an optional setter — an optional setter nothing called is
  exactly how the old struct stayed at "Unknown". Carries machine/memory/system IDs, core counts +
  SMT, cache sizes + detection method, TSC, installed vs OS-visible RAM, SIMD, virtualization +
  hypervisor, SMBIOS module list. Nothing newly detected: `generate_machine_id` and
  `memory_fingerprint` already ran on every invocation and were simply never stored.
- `RunConfigSnapshot` — request **vs resolved** throughout (`memory=60%` and `8 × 3.000 GiB on 2 MB
  pages` are different facts; only the outcome explains a throughput number). Memory spec +
  determinism, page-size mix, thread/CPU/NUMA assignment, execution + calibration snapshots, per-test
  window/chunk/reps/pattern/flush. Captured in `runner.rs` at the one point where it is *observed*
  rather than predicted.
- `--compare-results` prints differences **above** the percentages, ⛔ (invalidates) vs ⚠️ (worth
  knowing). **No backwards compatibility** (deliberate): every `#[serde(default)]` is gone, so a
  partial result file fails loudly instead of materialising zeros — the exact failure this item
  existed to kill. Pre-#67 baselines in `results/` no longer parse; delete them.
- JSON presentation trimmed at *serialisation* only (`formatting.rs`), in-memory arithmetic keeps
  full precision: whole MiB/s, 2 dp GiB, and `ByteSize` human units that only use a unit dividing
  the value **exactly**, so `"480 MiB"` round-trips and `49153` stays `"49153 B"`. The four
  `*_throughput_gib_s` fields were removed as duplicates (every producer computed `mib_s / 1024.0`);
  rounding had made the pair mutually inconsistent.

**Timestamp alignment (2026-09-09)** — the log and result files of a single run could not be paired.
`setup_logging()` used `Local::now()` and `TestRunResult::new()` used `Utc::now()`, sampled at
different moments (logging starts before allocation, results are built after `ThreadPool::new`), so
`results/TMR_2026-09-08_04-18-35.json` sat next to `logs/TMR_2026-09-08_14-18-23.log` — a 10 h zone
gap **and** a 12 s gap. Fixing the zone alone would have left the seconds skewed, so the fix is a
single captured instant (`run_context::run_start` / `run_file_stem`, `OnceLock<DateTime<Local>>`)
that both filenames derive from. Decided: **console + log are local time and say so** (the console
formatter had been printing `Local::now()` with a literal `Z`, claiming UTC; log files get a header
stating the zone and naming their paired result file, rather than repeating an offset on every line),
while **result files carry both** — `start_time_local` with offset for reading, `start_time_utc` for
central consolidation across machines, `start_time` unix for machines. `TestComparison` reports use
UTC, since the two files may come from different zones and UTC is the only stamp that orders them.

**Item 3 folded into #29, not left open here.** Where the results/log boundary belongs cannot be
decided without #29 (is the log a faithful console transcript?) *and* #7 (a GUI forces an
engine/presentation split, which makes the real question "what structured events does the engine
emit", with console/log/JSON/GUI as renderers over one stream). Deciding the file layout first would
mean redoing it when the GUI lands. See #29's "Absorbed from #67 item 3".
