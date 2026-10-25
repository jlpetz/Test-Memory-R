//! Run identity and resolved configuration, recorded into every result file (TODO #67 items 1/2).
//!
//! # Why this exists
//!
//! A result file used to hold nothing but timings, which made `--compare-results` confidently wrong. A 2026-07-31 baseline compared against
//! a 2026-09-08 run reported StuckBit "+10-20% faster" and Refresh "-14% slower" with no code change between them: the baseline had run **8
//! threads × 7 GiB** and the new run **8 threads × 3 GiB**. The smaller working set is more cache-resident (StuckBit up) while Refresh's
//! fixed 64 ms sleeps became a larger share of a shorter run (Refresh down). None of that was in either result file — it had to be
//! reconstructed by hand from `logs/`, and the comparison had no basis on which to warn.
//!
//! So: every result now carries **what machine ran it** ([`RunIdentity`]) and **what was actually allocated and executed**
//! ([`RunConfigSnapshot`]). Both are captured once per run, off any test path.
//!
//! # Request vs. resolved
//!
//! The distinction runs through this whole module and is the point of it. A *request* (`memory=60%`) and the *outcome* (`8 × 3.000 GiB on 2
//! MB pages`) are different facts, and only the outcome explains a throughput number. Where they can diverge, both are stored.

use std::sync::OnceLock;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::app_config::MachineIdentity;
use crate::constants::bytes_to_gib_f64 as to_gib;
use crate::formatting::{ByteSize, serialize_round_2dp};
use crate::smbios::MemoryModule;

// ===========================================================================
// Run instant — the one clock reading that names this run
// ===========================================================================

/// The single wall-clock instant identifying this run, captured on first call.
///
/// **Everything that names or stamps a run must read it from here.** The log filename (`main.rs: setup_logging`) and the result filename
/// (`results.rs: TestRunResult::new`) used to call `Local::now()` and `Utc::now()` *independently*, and not at the same moment: logging is
/// initialised near the top of `main`, while the result is built in `runner.rs` after allocation and `ThreadPool::new`. So the two
/// artifacts of one run disagreed twice over — `results/TMR_2026-09-08_04-18-35.json` next to `logs/TMR_2026-09-08_14-18-23.log`, a 10 h
/// zone difference *and* a 12 s difference — and could not be paired by name at all. Fixing only the timezone would have left the seconds
/// skewed, which is why this is a shared instant rather than two matching format strings.
///
/// Local, not UTC, because it is the stem of a filename a human browses. Result *files* carry both zones as fields (see
/// [`crate::results::TestRunMetadata`]); a filename can only pick one.
pub fn run_start() -> DateTime<Local> {
    static RUN_START: OnceLock<DateTime<Local>> = OnceLock::new();
    *RUN_START.get_or_init(Local::now)
}

/// Filename stem shared by this run's log and result files, e.g. `TMR_2026-09-08_14-18-23`.
pub fn run_file_stem() -> String {
    format!("TMR_{}", run_start().format("%Y-%m-%d_%H-%M-%S"))
}

// ===========================================================================
// Item 1 — run identity
// ===========================================================================

/// Which machine produced a result, in enough detail to tell "the code got faster" from "the hardware changed".
///
/// Nothing here is newly detected: [`crate::app_config::AppConfig::generate_machine_id`] and
/// [`crate::smbios::SmbiosData::memory_fingerprint`] already run on **every** invocation (via `is_calibration_valid`, which is why TMR can
/// print `Calibration: Stale (hardware changed)`). This type just hands the same facts to the result file, which never received them
/// before.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunIdentity {
    /// Hash of CPU vendor/brand/family/model/stepping + microcode + cache sizes + core count + base frequency + BIOS vendor/version/date.
    /// Same value that gates calibration reuse, so a mismatch here means calibration would also have been rejected.
    pub machine_id: String,

    /// Hash of the SMBIOS memory configuration. Tracked separately from `machine_id` because RAM can be swapped or retimed without touching
    /// the CPU — and that is exactly the change most likely to move a memory test's numbers.
    pub memory_id: String,

    /// SMBIOS Type 1 system UUID. Distinguishes a VM migration / re-launch from a hardware change when `machine_id` still matches.
    pub system_uuid: Option<String>,

    /// Readable counterpart to `machine_id` — when the hash mismatches, these fields say what actually differs instead of leaving two
    /// opaque hex strings.
    pub machine: MachineIdentity,

    pub physical_cores: usize,
    pub logical_cores: usize,
    /// Whether the CPU exposes SMT siblings at all. How many TMR *used* is in [`ThreadSnapshot::active_threads_per_core`] — a machine
    /// capability and a run choice.
    pub smt_available: bool,

    /// Cache geometry. Recorded because cache-relative extent and chunk modes are computed from these numbers, so two machines with the
    /// same `machine_id`-relevant CPU but different L3 (a different SKU, or a hypervisor presenting a different slice) will size tests
    /// differently.
    pub l1d_cache_per_core: ByteSize,
    pub l2_cache_per_core: ByteSize,
    pub l3_cache: ByteSize,
    /// Left as a plain number: 64 needs no scaling, and it is used as a stride/divisor rather than read as a capacity.
    pub cache_line_bytes: usize,
    /// How the cache sizes above were obtained (CPUID leaf, Windows API, or fallback defaults). A result sized off "Default fallback
    /// values" is not comparable with a properly detected one.
    pub cache_detection_method: String,

    /// TSC frequency in GHz. Every latency figure is derived from this, so a drift here rescales them all — which is why calibration
    /// already treats a change as invalidating.
    pub tsc_frequency_ghz: f64,

    /// Physically installed RAM (`GetPhysicallyInstalledSystemMemory`).
    #[serde(serialize_with = "serialize_round_2dp")]
    pub installed_memory_gib: f64,
    /// RAM the OS reports as usable (`GlobalMemoryStatusEx`). Lower than installed on machines that carve out firmware/graphics
    /// reservations.
    #[serde(serialize_with = "serialize_round_2dp")]
    pub os_visible_memory_gib: f64,

    /// SIMD instruction sets available at runtime. Auto-dispatch tests resolve their width from this, so it determines which code path a
    /// `*Auto` test actually ran.
    pub simd_capabilities: String,

    /// Running under a hypervisor (CPUID hypervisor-present bit), and its vendor if it identified itself. Relevant beyond bookkeeping: a
    /// hypervisor can hide NUMA nodes, present a partial L3, and make physical page availability (hence page-size mix) vary run to run.
    pub virtualized: bool,
    pub hypervisor: Option<String>,

    /// Number of memory modules SMBIOS reported, and their summed capacity.
    pub memory_modules_reported: usize,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub memory_reported_gib: f64,

    /// Per-module SMBIOS Type 17 detail.
    ///
    /// **Read `memory_identity_is_physical` before trusting this as DIMM identity.** Stored in full rather than hashed away, because on
    /// bare metal it is the single most useful thing to have stamped on a memory-test result (part number, speed, serial). On a VM it is
    /// firmware fiction: this EC2 box reports `1 module, 64.0 GB` for what is really multi-channel host DRAM, with no serial. Keeping the
    /// raw report and labelling it beats silently dropping it.
    pub memory_modules: Vec<MemoryModule>,

    /// Whether `memory_modules` describes real DIMMs (bare metal) or a synthetic firmware view (virtualized). `memory_id` stays useful
    /// either way — on a VM it changes when the instance is retyped, which is the signal worth having.
    pub memory_identity_is_physical: bool,
}

impl RunIdentity {
    /// Snapshot the current machine.
    ///
    /// Called from `TestRunResult::new()` rather than exposed as an optional setter, deliberately: the bug this fixes was a
    /// `SystemInfoSnapshot` full of `"Unknown"` / `0.0` placeholders that no code ever populated. Wiring it into construction means it
    /// cannot be forgotten again.
    ///
    /// Cost is a few Win32 calls, once per run, before any test thread exists. CPU/cache data and SMBIOS both come from process-wide
    /// caches, so the firmware tables are not read again here.
    pub fn detect() -> Self {
        let sys = crate::tests::get_system_info();
        let cache = sys.get_cache_info();
        let smbios = crate::smbios::get_smbios();

        // Installed vs OS-visible RAM. Failure is not worth aborting a run over — leave zeros, which read as "not recorded" next to a
        // populated `machine_id`.
        let (installed_memory_gib, os_visible_memory_gib) = match crate::memory::allocation_strategy::SystemMemoryInfo::gather() {
            Ok(info) => (to_gib(info.total_installed_bytes), to_gib(info.total_physical_bytes)),
            Err(e) => {
                log::warn!("Could not record system memory totals in result: {}", e);
                (0.0, 0.0)
            }
        };

        let virtualized = cache.is_virtual_machine;

        Self {
            machine_id: crate::app_config::AppConfig::generate_machine_id(sys, smbios),
            memory_id: smbios.memory_fingerprint(),
            system_uuid: if smbios.system.uuid.is_empty() { None } else { Some(smbios.system.uuid.clone()) },
            // Same constructor `AppConfig::update_identity` uses, so a result file's readable identity always corresponds to the
            // `machine_id` that gates calibration reuse.
            machine: MachineIdentity::detect(sys, smbios),
            physical_cores: sys.physical_cores,
            logical_cores: sys.logical_cores,
            smt_available: sys.has_hyperthreading,
            l1d_cache_per_core: cache.per_core_l1d.into(),
            l2_cache_per_core: cache.per_core_l2.into(),
            l3_cache: cache.l3_cache.into(),
            cache_line_bytes: cache.cache_line_size,
            cache_detection_method: cache.detection_method.clone(),
            tsc_frequency_ghz: cache.tsc_frequency_ghz,
            installed_memory_gib,
            os_visible_memory_gib,
            simd_capabilities: crate::detect_simd_capabilities(),
            virtualized,
            hypervisor: cache.hypervisor_name.clone(),
            memory_modules_reported: smbios.memory_modules.len(),
            memory_reported_gib: to_gib(smbios.total_memory_bytes()),
            memory_modules: smbios.memory_modules.clone(),
            memory_identity_is_physical: !virtualized,
        }
    }
}

// ===========================================================================
// Item 2 — resolved run configuration
// ===========================================================================

/// What was asked for and what the run actually got. Captured once, after allocation and thread creation, so the resolved figures are
/// measured rather than predicted.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunConfigSnapshot {
    /// `argv`, with the executable path reduced to its filename. The canonical `memory`/`threads` fields below are TMR's *interpretation*;
    /// this is the record of what was typed, and it also captures flags with no structured home (`--quick-test`, `cpu-stride=`,
    /// `skip-cores=`).
    ///
    /// `argv[0]` is stripped to the bare filename because result files get shared and the install directory is both useless to a reader and
    /// a way to leak a username. The filename itself is kept — it can carry a build tag (`tmr-avx512.exe`) that explains a result.
    pub command_line: Vec<String>,

    /// Config name from the loaded config's metadata, if the run was driven by one (JSON or TM5 `.cfg`). The *path* is in `command_line`;
    /// this is the config's own declared name, which is what community configs are known by (`"1usmus_v3"`).
    pub config_name: Option<String>,

    pub memory: MemorySnapshot,
    pub threads: ThreadSnapshot,
    pub execution: ExecutionSnapshot,

    /// Calibration that was live during the run. `None` means cache-relative windows fell back to CPUID cache sizes — a materially
    /// different sizing basis, so worth distinguishing.
    pub calibration: Option<CalibrationSnapshot>,

    /// One entry per test in the resolved plan. Stored at run level rather than repeated inside every cycle's results: this is
    /// configuration, and it does not change between cycles.
    pub tests: Vec<TestConfigSnapshot>,

    /// The tests the plan had that this CPU can't run, each once and why (TODO 72; left out by `runner::leave_out_what_the_cpu_lacks`, TODO
    /// 99): a plan has fewer steps on a CPU without AVX-512. Absent when none was, and in results written before it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests_left_out: Vec<LeftOutTest>,
}

/// A test the run left out (`RunConfigSnapshot::tests_left_out`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct LeftOutTest {
    pub name: String,
    /// What the CPU lacks, e.g. `width 512 needs AVX-512, which this CPU lacks`
    pub reason: String,
}

/// Memory request, and the allocation that came back.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemorySnapshot {
    /// Canonical `memory=` spec reconstructed from the parsed strategy, e.g. `"20%-from-available"`.
    pub spec: String,

    /// Whether `spec` yields the same size on every run of this machine.
    ///
    /// **`false` is the trap that motivated this whole module.** `-from-available` sizes off *currently free* memory: two `--quick-test`
    /// runs minutes apart computed `12.414 GiB` and `11.996 GiB` raw and only agreed on 3.000 GiB/thread after rounding. That is luck, not
    /// reproducibility. Only `-from-total` and `-target` are deterministic.
    pub spec_is_deterministic: bool,

    /// Human-readable strategy label from the allocator (e.g.
    /// `"Reserve from Available [thread-aware (8×3.00 GiB)]"`).
    pub allocation_type: String,

    /// The figure the spec's percentage was applied to (total or available, per the mode).
    #[serde(serialize_with = "serialize_round_2dp")]
    pub reference_gib: f64,
    /// Bytes the plan asked for.
    #[serde(serialize_with = "serialize_round_2dp")]
    pub requested_gib: f64,
    /// Bytes deliberately left to the OS.
    #[serde(serialize_with = "serialize_round_2dp")]
    pub reserve_gib: f64,

    /// Free memory and load at the moment the plan was computed. For a non-deterministic spec this is the input that decided the size, so
    /// it is the only way to explain why two runs differ.
    #[serde(serialize_with = "serialize_round_2dp")]
    pub available_at_start_gib: f64,
    pub memory_load_percent_at_start: u32,

    /// What actually came back, summed over every allocated block. May be below `requested_gib`: a partially satisfied run still produces
    /// results (one observed run got 7 of 8 blocks).
    #[serde(serialize_with = "serialize_round_2dp")]
    pub allocated_gib: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub per_thread_gib: f64,
    /// The requests the allocator made to Windows, and how many it refused (the allocation report's Requests and Refused); absent from
    /// results written before them.
    #[serde(default)]
    pub requests: usize,
    #[serde(default)]
    pub refused: usize,

    /// The API that allocated the memory. The page sizes it actually got are `page_mix`.
    pub backend: String,
    /// Whether `SeLockMemoryPrivilege` was held. Large pages need it, but holding it does not mean they were obtained; `page_mix` says what
    /// was.
    pub large_pages_available: bool,
    pub min_page_size: String,
    pub max_page_size: String,

    pub page_mix: PageSizeMix,
}

/// How allocated bytes split across page sizes.
///
/// Not cosmetic, and not stable between runs: the allocator takes 1 GB huge pages when physical memory is contiguous enough and falls back
/// to 2 MB otherwise, so the same request lands differently as fragmentation changes. One observed pair of same-size runs differed 22 GiB
/// vs 1 GiB on huge pages, purely from uptime. TLB reach differs sharply between the two, so **two runs with identical totals are not
/// automatically comparable** — this is what says so.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PageSizeMix {
    #[serde(serialize_with = "serialize_round_2dp")]
    pub huge_1gib_gib: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub large_2mib_gib: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub regular_4kib_gib: f64,
}

impl PageSizeMix {
    /// The whole run's bytes on each page size, from the allocator's spans: a whole-run property, because it is driven by physical
    /// contiguity, not by thread.
    pub fn from_spans(spans: &[crate::memory::stitched::ThreadSpan]) -> Self {
        use crate::memory::allocator::PageSizeLevel;
        let gib = |page| to_gib(spans.iter().map(|s| s.bytes_at(page) as u64).sum());
        Self {
            huge_1gib_gib: gib(PageSizeLevel::Huge),
            large_2mib_gib: gib(PageSizeLevel::Large),
            regular_4kib_gib: gib(PageSizeLevel::Regular),
        }
    }

    /// Compact one-line summary, e.g. `"18.00 GiB 1 GiB pages, 10.00 GiB 2 MiB pages"`.
    pub fn describe(&self) -> String {
        let parts: Vec<String> = [(self.huge_1gib_gib, "1 GiB"), (self.large_2mib_gib, "2 MiB"), (self.regular_4kib_gib, "4 KiB")]
            .iter()
            .filter(|&&(gib, _)| gib > 0.0)
            .map(|&(gib, page)| format!("{gib:.2} GiB {page} pages"))
            .collect();
        if parts.is_empty() { "none".to_string() } else { parts.join(", ") }
    }
}

/// Worker-thread placement. Recorded per thread because *which* CPUs ran matters as much as how many: on a multi-CCD part, packed versus
/// spread placement changes throughput by 13-25%.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThreadSnapshot {
    pub thread_count: usize,
    /// Whether workers were pinned to specific logical CPUs. Unpinned, the OS scheduler may move a thread across cache or NUMA domains
    /// mid-test and the numbers stop meaning much.
    pub pinned: bool,
    /// Worker scheduling priority (`normal`, `high` or `realtime`). Under contention from other processes a lower one loses time slices,
    /// which shows up as lower throughput.
    pub thread_priority: String,
    /// `(thread_id, logical_cpu, numa_node)` as actually assigned.
    pub assignments: Vec<ThreadAssignment>,
    /// Distinct NUMA nodes used, and the thread count on each.
    pub numa_distribution: Vec<NumaThreadCount>,
    /// SMT siblings TMR treated as active (1 or 2). This is the **divisor** for L1/L2-relative extent and chunk sizes, so the same `Cache
    /// (L2)` spec resolves to half the bytes at 2 — making it a direct input to what every cache-tier test measured.
    pub active_threads_per_core: usize,
    /// Resolved logical-CPU pool the pool drew from (`cpus=` / `skip-cores=` / `cpu-stride=` already applied). `None` means no restriction
    /// was given.
    pub cpu_pool: Option<Vec<usize>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ThreadAssignment {
    pub thread_id: usize,
    pub logical_cpu: usize,
    pub numa_node: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct NumaThreadCount {
    pub numa_node: u32,
    pub threads: usize,
}

/// Suite-level execution settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExecutionSnapshot {
    /// Cycles requested. The count actually completed is `overall_stats.cycles_completed`, which is lower on an interrupted run — comparing
    /// an interrupted run to a complete one is a mistake worth being able to detect.
    pub cycles_requested: Option<u32>,
    pub duration_secs_requested: Option<u32>,
    pub error_mode: String,
    /// Glob filter that selected the plan (`test=Mem-StuckBit*`), if any.
    pub test_filter: Option<String>,
    /// The built-in plan the tests came from (`stability`, TODO 25); `None` when a config chose them, or in results written before plans.
    pub plan: Option<String>,
    /// Configured memory channels. Feeds SimpleTest's stride formula, so it changes the access pattern rather than just labelling it.
    pub channels: u32,
    /// The pattern spec SimpleTest's native programs follow (`crate::pattern::VERSION`); absent from results written before them.
    #[serde(default)]
    pub pattern_version: u32,
    /// The native patterns' run seed, as `seed=` takes it (`0x...`): with the same layout, the run's bytes again.
    #[serde(default)]
    pub pattern_seed: Option<String>,
    /// The transfer width the switch pattern flipped at, and where it came from (`beat=`, SMBIOS's memory type, or unknown, when it
    /// alternated 64 and 32).
    #[serde(default)]
    pub transfer_bits: Option<u32>,
    #[serde(default)]
    pub transfer_source: Option<String>,
    /// Whether failed checks were rescanned to name each bad word (`rescan=`).
    #[serde(default)]
    pub rescan: Option<bool>,
    /// The run's SIMD width (TODO 99, `crate::simd::test_width`), the one given or, under auto, the widest the CPU has: what the tests that
    /// take the run's width run at. Each records its own (`TestConfigSnapshot::simd_width_bits`): Mem-Sequence runs 256 for 128, and a
    /// Bench-*-Seal-{TMR,TM5} the seal's auto under auto. Absent in results written before it.
    #[serde(default)]
    pub simd_width_bits: Option<u32>,
    /// Where the run's width came from: `width=`, `the config's width`, or `auto` when none was given.
    #[serde(default)]
    pub simd_width_source: Option<String>,
    /// The seal's kernel, its pattern and width as the plan names it (`tmr-256`), and where the width came from (TODO 99): `calibrated`
    /// (this machine's `--calibrate-seal`), `rule` (auto without one: 256 with AVX2), or the setting (`seal-width=`, `width=`, the config's
    /// `seal_width` or `width`). `None` in an unsealed run.
    #[serde(default)]
    pub seal: Option<String>,
    #[serde(default)]
    pub seal_width_source: Option<String>,
}

/// The calibration that was live during the run. Cache-relative windows are sized from these measured tier boundaries, so a result is only
/// comparable with another sized off the same ones.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CalibrationSnapshot {
    /// When the calibration was measured (ISO 8601 UTC).
    pub timestamp: String,
    /// Page size the calibration allocation used — a calibration run on 4 KB pages describes different tier boundaries than one on 2 MB.
    pub page_size: String,
    /// Measured optimal working-set size per tier, which is literally the number extent sizing multiplies by the test's scale factor.
    pub tiers: Vec<CalibratedTier>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CalibratedTier {
    pub tier: String,
    pub optimal_size: ByteSize,
    /// Median latency measured at `optimal_size`. Recorded alongside the size because it is the evidence for it — a tier whose latency
    /// looks wrong explains an extent size that looks wrong.
    #[serde(serialize_with = "serialize_round_2dp")]
    pub median_latency_ns: f64,
}

/// Per-test configuration as resolved for this run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestConfigSnapshot {
    /// Registered/display name, with auto-dispatch already resolved (`Mem-StuckBit512_A`), so it matches the name results are keyed by.
    pub name: String,
    /// Underlying test function's own name. Several registrations share one function (the flush and `_A` variants), so this is not unique —
    /// it says which code path ran.
    pub function: String,
    /// Stage-2 per-test working set.
    pub extent_mode: String,
    /// Stage-3 iteration unit within the extent.
    pub chunk_mode: String,
    /// What those came to, on the thread with the least memory (threads differ only when allocation fell short, which the allocation report
    /// flags): the bytes covered, the chunk, how many chunks, and how much they overlap to cover the extent.
    #[serde(default)]
    pub extent_bytes: u64,
    #[serde(default)]
    pub chunk_bytes: u64,
    #[serde(default)]
    pub chunks: usize,
    #[serde(default)]
    pub overlap_bytes: u64,
    /// The test's limits: it stops at whichever comes first (none: no such limit). The cycles a step completed, and which limit ended it,
    /// are in its result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_secs: Option<u32>,
    pub verify_reps: u32,
    pub test_reps: u32,
    pub write_read_cycles: u32,
    pub pattern_mode: Option<u32>,
    /// The native program a SimpleTest or Mem-Sequence ran (pattern spec v2), when no `pattern_mode` was set.
    #[serde(default)]
    pub program: Option<Vec<String>>,
    /// Mem-Sequence's passes, and the order of the steps that name none
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<String>,
    /// Whether the verify was forced through DRAM with CLFLUSHOPT. Costs real bandwidth by design, so a flushed and an unflushed run of the
    /// same test are not comparable.
    pub flush_before_verify: bool,
    /// Whether this test relied on a previous test's pattern instead of writing its own.
    pub skip_init: bool,
    /// The SIMD width the test ran at, where its name alone doesn't fix it (TODO 99, `TestDefinition::simd_width`): an `-Auto` test,
    /// Mem-Sequence (256 for 128, which it has no kernel for) and the Bench pairs that name no width. Absent for a test that runs at the
    /// width it is named for or has none, and in results written before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simd_width_bits: Option<u32>,
}

// ===========================================================================
// Capture
// ===========================================================================

/// Everything the runner already knows at capture time, bundled so the signature stays readable (and clippy's `too_many_arguments` stays
/// quiet without an `allow`).
pub struct RunConfigInputs<'a> {
    /// `argv` as the process got it, before the CLI adds its defaults.
    pub command_line: &'a [String],
    /// `layout.system_memory_info` — free memory and load as observed when the plan was computed.
    pub system_memory_info: &'a crate::memory::allocation_strategy::SystemMemoryInfo,
    /// `layout.allocation_result` — the *requested* figures.
    pub allocation_result: &'a crate::memory::allocation_strategy::AllocationResult,
    /// `layout.strategy` — the parsed `memory=` spec.
    pub strategy: &'a crate::memory::allocation_strategy::EnhancedMemoryStrategy,
    pub runtime_config: &'a crate::RuntimeConfig,
    /// From the allocator's spans, so it reflects the outcome.
    pub page_mix: PageSizeMix,
    /// The requests made to Windows, and how many were refused.
    pub requests: usize,
    pub refused: usize,
    /// The smallest thread's memory, which the tests' resolved layouts are worked out on.
    pub thread_memory_least: usize,
    pub allocated_bytes: u64,
    pub thread_count: usize,
    pub cpu_assignments: &'a [crate::thread_pool::CpuAssignment],
    pub pinned: bool,
    pub thread_priority: crate::runner::ThreadPriority,
    pub suite_timing: &'a crate::runner::TestSuiteTiming,
    pub error_mode: crate::ErrorMode,
    pub test_filter: Option<String>,
    pub plan: Option<String>,
    pub channels: u32,
    pub config_name: Option<String>,
    /// The resolved plan — after filtering, overrides, and auto-dispatch.
    pub tests: &'a [crate::runner::TestDefinition],
    /// The tests left out of it, by name, and why (`runner::leave_out_what_the_cpu_lacks`).
    pub left_out: &'a [(String, String)],
    pub cache_info: &'a crate::cache::CacheInfo,
    /// The seal's kernel and where its width came from, in a sealed run.
    pub seal: Option<(crate::seal::SealKernel, crate::seal::SealWidthSource)>,
}

impl RunConfigSnapshot {
    /// Build the snapshot. Call after allocation and thread-pool creation so the resolved figures are observed rather than predicted, and
    /// before the first cycle starts.
    pub fn capture(inputs: RunConfigInputs<'_>) -> Self {
        let RunConfigInputs {
            command_line,
            system_memory_info,
            allocation_result: allocation,
            strategy,
            runtime_config,
            page_mix,
            requests,
            refused,
            thread_memory_least,
            allocated_bytes,
            thread_count,
            cpu_assignments,
            pinned,
            thread_priority,
            suite_timing,
            error_mode,
            test_filter,
            plan,
            channels,
            config_name,
            tests,
            left_out,
            cache_info,
            seal,
        } = inputs;

        let memory = MemorySnapshot {
            spec: strategy.describe_spec(),
            spec_is_deterministic: strategy.allocation_mode.is_deterministic(),
            allocation_type: allocation.allocation_type.clone(),
            reference_gib: to_gib(allocation.reference_bytes),
            requested_gib: to_gib(allocation.allocation_bytes),
            reserve_gib: to_gib(allocation.reserve_bytes),
            available_at_start_gib: to_gib(system_memory_info.available_physical_bytes),
            memory_load_percent_at_start: system_memory_info.memory_load_percent,
            allocated_gib: to_gib(allocated_bytes),
            per_thread_gib: if thread_count > 0 { to_gib(allocated_bytes / thread_count as u64) } else { 0.0 },
            requests,
            refused,
            backend: format!("{:?}", runtime_config.memory_backend),
            large_pages_available: runtime_config.large_pages_available,
            min_page_size: runtime_config.memory_allocation.min_page_size.clone(),
            max_page_size: runtime_config.memory_allocation.max_page_size.clone(),
            page_mix,
        };

        // NUMA distribution, kept in node order so two runs diff cleanly.
        let mut numa_distribution: Vec<NumaThreadCount> = Vec::new();
        for &(_, _, numa_node) in cpu_assignments {
            match numa_distribution.iter_mut().find(|n| n.numa_node == numa_node) {
                Some(entry) => entry.threads += 1,
                None => numa_distribution.push(NumaThreadCount { numa_node, threads: 1 }),
            }
        }
        numa_distribution.sort_by_key(|n| n.numa_node);

        let threads = ThreadSnapshot {
            thread_count,
            pinned,
            thread_priority: thread_priority.to_string(),
            assignments: cpu_assignments
                .iter()
                .map(|&(thread_id, logical_cpu, numa_node)| ThreadAssignment { thread_id, logical_cpu, numa_node })
                .collect(),
            numa_distribution,
            active_threads_per_core: crate::tests::get_active_threads_per_core(),
            cpu_pool: runtime_config.cpu_list.clone(),
        };

        let execution = ExecutionSnapshot::capture(suite_timing, error_mode, test_filter, plan, channels, seal);

        let calibration = crate::tests::get_calibration_data().map(|cal| {
            let mut tiers: Vec<CalibratedTier> = cal
                .tiers
                .iter()
                .map(|(tier, result)| CalibratedTier {
                    tier: format!("{:?}", tier),
                    optimal_size: result.optimal_size.into(),
                    median_latency_ns: result.median_latency_ns,
                })
                .collect();
            // HashMap iteration order is arbitrary; sort so results diff cleanly.
            tiers.sort_by(|a, b| a.tier.cmp(&b.tier));
            CalibrationSnapshot { timestamp: cal.timestamp.to_rfc3339(), page_size: cal.page_size.clone(), tiers }
        });

        let tests = tests.iter().map(|def| TestConfigSnapshot::capture(def, cache_info, thread_count, thread_memory_least)).collect();

        Self {
            command_line: capture_command_line(command_line),
            config_name,
            memory,
            threads,
            execution,
            calibration,
            tests,
            tests_left_out: left_out.iter().map(|(name, reason)| LeftOutTest { name: name.clone(), reason: reason.clone() }).collect(),
        }
    }
}

impl ExecutionSnapshot {
    /// The run's suite-level settings (`RunConfigSnapshot::capture`); `seal` is the seal's kernel and where its width came from, in a
    /// sealed run.
    fn capture(
        suite_timing: &crate::runner::TestSuiteTiming, error_mode: crate::ErrorMode, test_filter: Option<String>, plan: Option<String>,
        channels: u32, seal: Option<(crate::seal::SealKernel, crate::seal::SealWidthSource)>,
    ) -> Self {
        ExecutionSnapshot {
            cycles_requested: suite_timing.global_cycles,
            duration_secs_requested: suite_timing.global_duration_secs,
            error_mode: format!("{:?}", error_mode),
            test_filter,
            plan,
            channels,
            pattern_version: crate::pattern::VERSION,
            pattern_seed: Some(format!("{:#018x}", crate::pattern::run_seed())),
            transfer_bits: crate::pattern::transfer_beat().0.map(|b| b.bits()),
            transfer_source: Some(crate::pattern::transfer_beat().1.to_string()),
            rescan: Some(crate::pattern::rescan_enabled()),
            simd_width_bits: Some(crate::simd::test_width().bits()),
            simd_width_source: Some(crate::simd::width_source().to_string()),
            seal: seal.map(|(kernel, _)| kernel.name()),
            seal_width_source: seal.map(|(_, source)| source.name().to_string()),
        }
    }
}

impl TestConfigSnapshot {
    /// One test as the run resolved it (`RunConfigSnapshot::capture`), its layout on the thread with the least memory,
    /// `thread_memory_least`.
    fn capture(
        def: &crate::runner::TestDefinition, cache_info: &crate::cache::CacheInfo, thread_count: usize, thread_memory_least: usize,
    ) -> Self {
        // Extent/chunk modes are rendered with the same formatter the console tables use, so the recorded string matches what the user saw
        // — including the resolved byte size for cache specs, which is where the SMT divisor becomes visible.
        use crate::reporting::formatters::ReportFormatter;
        let formatter = crate::reporting::formatters::DefaultFormatter::new();
        let layout = crate::test_memory::resolve_layout(&def.config, def.actual_name, thread_memory_least);
        let (extent, spread) = layout.map_or((0, None), |(extent, spread)| (extent, Some(spread)));
        TestConfigSnapshot {
            name: def.display_name.clone(),
            function: def.actual_name.to_string(),
            extent_mode: formatter.format_extent_mode_with_size(&def.config.extent_mode, cache_info, thread_count),
            chunk_mode: formatter.format_chunk_mode_with_size(&def.config.chunk_mode, cache_info, thread_count),
            verify_reps: def.config.verify_reps,
            test_reps: def.config.test_reps,
            write_read_cycles: def.config.write_read_cycles,
            pattern_mode: def.config.pattern_mode,
            program: (def.actual_name.starts_with("Mem-SimpleV2") || def.actual_name == "Mem-Sequence")
                .then(|| def.config.native_program().map(|p| p.names()))
                .flatten(),
            sequence: (def.actual_name == "Mem-Sequence")
                .then(|| def.config.sequence())
                .map(|s| format!("{} (order {})", s.name(), s.order.name())),
            flush_before_verify: def.config.flush_before_verify,
            skip_init: def.config.skip_init,
            extent_bytes: extent as u64,
            chunk_bytes: spread.as_ref().map_or(0, |s| s.chunk() as u64),
            // The page exchange's tiles sit apart, the last one shorter (TODO 28)
            chunks: spread
                .as_ref()
                .map_or(0, |s| if crate::exchange::is_exchange(def.actual_name) { extent.div_ceil(s.chunk().max(1)) } else { s.count() }),
            overlap_bytes: spread
                .as_ref()
                .map_or(0, |s| if crate::exchange::is_exchange(def.actual_name) { 0 } else { s.overlap() as u64 }),
            cycle_limit: def.config.timing.cycles,
            time_limit_secs: def.config.timing.duration_secs,
            simd_width_bits: def.simd_width().map(crate::simd::Width::bits),
        }
    }
}

/// `argv` with the executable path reduced to its filename.
///
/// Only `argv[0]` is rewritten. Later arguments are left verbatim — they are what the user typed, and `config=` in particular needs its
/// path to stay meaningful. A caller who passes an absolute config path still puts a directory in the record; that is their own text, not
/// something TMR injected on their behalf.
fn capture_command_line(argv: &[String]) -> Vec<String> {
    let mut args = argv.to_vec();
    if let Some(exe) = args.first_mut()
        && let Some(name) = std::path::Path::new(exe.as_str()).file_name().and_then(|n| n.to_str())
    {
        *exe = name.to_string();
    }
    args
}

// ===========================================================================
// Comparison
// ===========================================================================

/// A single difference between two runs that undermines a like-for-like comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDifference {
    /// What differs, e.g. `"Machine ID"`.
    pub field: String,
    pub baseline: String,
    pub current: String,
    /// `true` when the difference alone is enough to invalidate the throughput comparison, as opposed to being worth noting. Drives ⛔ vs ⚠️
    /// in the report.
    pub invalidates: bool,
}

impl RunDifference {
    pub fn new(field: &str, baseline: impl Into<String>, current: impl Into<String>, invalidates: bool) -> Self {
        Self { field: field.to_string(), baseline: baseline.into(), current: current.into(), invalidates }
    }
}

/// Percentage difference that counts as a real size change rather than rounding.
///
/// Allocation sizes are rounded to page and block boundaries, so two runs of an identical spec can land a hair apart. 1% is well below any
/// threshold at which working-set size moves throughput, and well above that rounding.
const SIZE_TOLERANCE_PERCENT: f64 = 1.0;

fn differs_materially(baseline: f64, current: f64) -> bool {
    if baseline == 0.0 && current == 0.0 {
        return false;
    }
    let reference = baseline.abs().max(current.abs());
    ((current - baseline).abs() / reference) * 100.0 > SIZE_TOLERANCE_PERCENT
}

/// Compare the identity of two runs.
///
/// Hardware differences are reported as *invalidating*: if the CPU, RAM, or cache geometry changed then a throughput delta says nothing
/// about the code, which is exactly the mistake that motivated TODO #67.
pub fn compare_identity(baseline: &RunIdentity, current: &RunIdentity) -> Vec<RunDifference> {
    let mut diffs = Vec::new();

    if baseline.machine_id != current.machine_id {
        diffs.push(RunDifference::new("Machine ID (CPU/cache/BIOS)", &baseline.machine_id, &current.machine_id, true));
        // The hash is opaque, so name the fields that actually moved.
        let m = (&baseline.machine, &current.machine);
        if m.0.cpu_brand != m.1.cpu_brand {
            diffs.push(RunDifference::new("  CPU", &m.0.cpu_brand, &m.1.cpu_brand, true));
        }
        if m.0.cpu_microcode != m.1.cpu_microcode {
            diffs.push(RunDifference::new("  Microcode", &m.0.cpu_microcode, &m.1.cpu_microcode, true));
        }
        if m.0.bios_version != m.1.bios_version {
            diffs.push(RunDifference::new("  BIOS version", &m.0.bios_version, &m.1.bios_version, true));
        }
        if m.0.system_product != m.1.system_product {
            diffs.push(RunDifference::new("  System product", &m.0.system_product, &m.1.system_product, true));
        }
        if m.0.physical_cores != m.1.physical_cores {
            diffs.push(RunDifference::new("  Physical cores", m.0.physical_cores.to_string(), m.1.physical_cores.to_string(), true));
        }
    }

    if baseline.memory_id != current.memory_id {
        // Deliberately separate from machine_id: RAM can change with the CPU untouched, and for a memory tester that is the more
        // consequential half.
        diffs.push(RunDifference::new("Memory fingerprint", &baseline.memory_id, &current.memory_id, true));
        if baseline.memory_reported_gib != current.memory_reported_gib {
            diffs.push(RunDifference::new(
                "  Reported capacity",
                format!("{:.1} GiB", baseline.memory_reported_gib),
                format!("{:.1} GiB", current.memory_reported_gib),
                true,
            ));
        }
        if !baseline.memory_identity_is_physical || !current.memory_identity_is_physical {
            diffs.push(RunDifference::new(
                "  (virtualized)",
                "SMBIOS memory detail is a firmware view, not DIMM identity",
                "instance retype is the likely cause",
                false,
            ));
        }
    }

    if baseline.system_uuid != current.system_uuid {
        // Same hardware model, different machine or VM instance. Not invalidating by itself — two identical hosts are a fair comparison —
        // but it explains run-to-run noise.
        diffs.push(RunDifference::new(
            "System UUID",
            baseline.system_uuid.clone().unwrap_or_else(|| "none".into()),
            current.system_uuid.clone().unwrap_or_else(|| "none".into()),
            false,
        ));
    }

    if baseline.l3_cache != current.l3_cache {
        diffs.push(RunDifference::new("L3 cache", baseline.l3_cache.to_string(), current.l3_cache.to_string(), true));
    }

    if baseline.simd_capabilities != current.simd_capabilities {
        // Auto-dispatch resolves width from this, so a change can silently swap the code path under a test whose name did not change.
        diffs.push(RunDifference::new("SIMD capabilities", &baseline.simd_capabilities, &current.simd_capabilities, true));
    }

    if differs_materially(baseline.tsc_frequency_ghz, current.tsc_frequency_ghz) {
        // Every latency number is scaled by this.
        diffs.push(RunDifference::new(
            "TSC frequency",
            format!("{:.4} GHz", baseline.tsc_frequency_ghz),
            format!("{:.4} GHz", current.tsc_frequency_ghz),
            true,
        ));
    }

    if baseline.cache_detection_method != current.cache_detection_method {
        diffs.push(RunDifference::new("Cache detection", &baseline.cache_detection_method, &current.cache_detection_method, false));
    }

    diffs
}

/// Compare the resolved configuration of two runs.
///
/// Working-set size and thread count dominate throughput, so a difference in either makes the per-test percentages meaningless regardless
/// of what the code did in between.
pub fn compare_config(b: &RunConfigSnapshot, c: &RunConfigSnapshot) -> Vec<RunDifference> {
    let mut diffs = Vec::new();

    if differs_materially(b.memory.allocated_gib, c.memory.allocated_gib) {
        diffs.push(RunDifference::new(
            "Total allocation",
            format!("{:.3} GiB", b.memory.allocated_gib),
            format!("{:.3} GiB", c.memory.allocated_gib),
            true,
        ));
    }

    if differs_materially(b.memory.per_thread_gib, c.memory.per_thread_gib) {
        // The one that actually bit us: same test, different per-thread working set, so cache residency differs and the throughput delta is
        // about size, not code.
        diffs.push(RunDifference::new(
            "Per-thread allocation",
            format!("{:.3} GiB", b.memory.per_thread_gib),
            format!("{:.3} GiB", c.memory.per_thread_gib),
            true,
        ));
    }

    if b.threads.thread_count != c.threads.thread_count {
        diffs.push(RunDifference::new("Thread count", b.threads.thread_count.to_string(), c.threads.thread_count.to_string(), true));
    }

    if b.threads.active_threads_per_core != c.threads.active_threads_per_core {
        // Halves or doubles every L1/L2-relative extent and chunk size.
        diffs.push(RunDifference::new(
            "Active threads per core (SMT)",
            b.threads.active_threads_per_core.to_string(),
            c.threads.active_threads_per_core.to_string(),
            true,
        ));
    }

    if b.threads.assignments != c.threads.assignments && b.threads.thread_count == c.threads.thread_count {
        let cpus = |s: &ThreadSnapshot| s.assignments.iter().map(|a| a.logical_cpu.to_string()).collect::<Vec<_>>().join(",");
        // Same count, different placement. On a multi-CCD part this alone moves throughput 13-25%, so it is worth flagging even though it
        // is not a config error.
        diffs.push(RunDifference::new("CPU placement", cpus(&b.threads), cpus(&c.threads), false));
    }

    if b.threads.numa_distribution != c.threads.numa_distribution {
        let nodes = |s: &ThreadSnapshot| {
            s.numa_distribution.iter().map(|n| format!("node{}×{}", n.numa_node, n.threads)).collect::<Vec<_>>().join(" ")
        };
        diffs.push(RunDifference::new("NUMA distribution", nodes(&b.threads), nodes(&c.threads), false));
    }

    if b.threads.pinned != c.threads.pinned {
        diffs.push(RunDifference::new(
            "Thread pinning",
            if b.threads.pinned { "pinned" } else { "unpinned" },
            if c.threads.pinned { "pinned" } else { "unpinned" },
            true,
        ));
    }

    if b.threads.thread_priority != c.threads.thread_priority {
        diffs.push(RunDifference::new("Thread priority", &b.threads.thread_priority, &c.threads.thread_priority, false));
    }

    if b.memory.page_mix.describe() != c.memory.page_mix.describe() {
        // Not a config difference — an environment one. The allocator takes what physical contiguity allows, so this varies with uptime
        // even for an identical command line.
        diffs.push(RunDifference::new("Page-size mix", b.memory.page_mix.describe(), c.memory.page_mix.describe(), false));
    }

    if b.memory.spec != c.memory.spec {
        diffs.push(RunDifference::new("Memory spec", &b.memory.spec, &c.memory.spec, false));
    }

    if !b.memory.spec_is_deterministic || !c.memory.spec_is_deterministic {
        // Even when the sizes happened to match, say so: it was not guaranteed to, so a future re-run of the same command need not
        // reproduce either number.
        diffs.push(RunDifference::new(
            "Memory spec reproducibility",
            if b.memory.spec_is_deterministic { "deterministic" } else { "depends on free memory" },
            if c.memory.spec_is_deterministic { "deterministic" } else { "depends on free memory" },
            false,
        ));
    }

    if b.execution.cycles_requested != c.execution.cycles_requested {
        let fmt = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_else(|| "unlimited".into());
        diffs.push(RunDifference::new("Cycles requested", fmt(b.execution.cycles_requested), fmt(c.execution.cycles_requested), false));
    }

    if b.execution.channels != c.execution.channels {
        // Changes SimpleTest's stride, i.e. the access pattern itself.
        diffs.push(RunDifference::new("Memory channels", b.execution.channels.to_string(), c.execution.channels.to_string(), true));
    }

    if b.memory.backend != c.memory.backend {
        diffs.push(RunDifference::new("Memory backend", &b.memory.backend, &c.memory.backend, true));
    }

    // The patterns: a SimpleTest on another spec, width or rescan setting tests something else
    if b.execution.pattern_version != c.execution.pattern_version {
        let v = |n: u32| if n == 0 { "1 (before native patterns)".to_string() } else { n.to_string() };
        diffs.push(RunDifference::new("Pattern spec", v(b.execution.pattern_version), v(c.execution.pattern_version), true));
    }
    if b.execution.transfer_bits != c.execution.transfer_bits {
        let w = |bits: Option<u32>| bits.map_or("unknown".to_string(), |b| format!("{b} bits"));
        diffs.push(RunDifference::new("Transfer width", w(b.execution.transfer_bits), w(c.execution.transfer_bits), true));
    }
    if b.execution.rescan != c.execution.rescan {
        let r = |on: Option<bool>| on.map_or("-".to_string(), |on| if on { "on".to_string() } else { "off".to_string() });
        diffs.push(RunDifference::new("Rescan", r(b.execution.rescan), r(c.execution.rescan), false));
    }

    // Per-test config, for tests present in both plans. An extent/chunk/flush change explains a single test moving while its neighbours did
    // not — otherwise easy to misread as a regression.
    for cur_test in &c.tests {
        let Some(base_test) = b.tests.iter().find(|t| t.name == cur_test.name) else {
            continue;
        };
        let mut note = |field: &str, bv: String, cv: String| {
            diffs.push(RunDifference::new(&format!("{}: {}", cur_test.name, field), bv, cv, true));
        };
        if base_test.extent_mode != cur_test.extent_mode {
            note("extent", base_test.extent_mode.clone(), cur_test.extent_mode.clone());
        }
        if base_test.chunk_mode != cur_test.chunk_mode {
            note("chunk", base_test.chunk_mode.clone(), cur_test.chunk_mode.clone());
        }
        if base_test.flush_before_verify != cur_test.flush_before_verify {
            note("flush before verify", base_test.flush_before_verify.to_string(), cur_test.flush_before_verify.to_string());
        }
        if base_test.verify_reps != cur_test.verify_reps {
            note("verify reps", base_test.verify_reps.to_string(), cur_test.verify_reps.to_string());
        }
        if base_test.pattern_mode != cur_test.pattern_mode || base_test.program != cur_test.program {
            let p = |t: &TestConfigSnapshot| match (&t.program, t.pattern_mode) {
                (Some(program), _) => program.join("+"),
                (None, Some(mode)) => format!("mode {mode}"),
                (None, None) => "-".to_string(),
            };
            note("pattern", p(base_test), p(cur_test));
        }
        if base_test.test_reps != cur_test.test_reps {
            note("test reps", base_test.test_reps.to_string(), cur_test.test_reps.to_string());
        }
        if base_test.write_read_cycles != cur_test.write_read_cycles {
            note("write/read cycles", base_test.write_read_cycles.to_string(), cur_test.write_read_cycles.to_string());
        }
        if base_test.function != cur_test.function {
            // Same registered name, different implementation — e.g. auto-dispatch resolved to a different SIMD width because the machine
            // changed.
            note("implementation", base_test.function.clone(), cur_test.function.clone());
        }
        // A test that takes the run's width under one name, as Mem-Sequence and the Bench pairs do (TODO 99); results written before the
        // record have none
        if let (Some(b), Some(c)) = (base_test.simd_width_bits, cur_test.simd_width_bits)
            && b != c
        {
            note("SIMD width", format!("{b} bits"), format!("{c} bits"));
        }
    }

    diffs
}

/// Render identity + config differences as a block for the comparison report. Empty string when the two runs are equivalent, so callers can
/// append unconditionally.
pub fn render_differences(diffs: &[RunDifference]) -> String {
    if diffs.is_empty() {
        return String::new();
    }

    let invalidating = diffs.iter().filter(|d| d.invalidates).count();
    let mut out = String::new();

    if invalidating > 0 {
        out.push_str(
            "⛔ NOT A LIKE-FOR-LIKE COMPARISON — the runs differ in ways that change performance\n   \
             independently of any code change. Treat the percentages below as unattributable.\n\n",
        );
    } else {
        out.push_str("⚠️  Run differences worth knowing about (none invalidate the comparison):\n\n");
    }

    let width = diffs.iter().map(|d| d.field.len()).max().unwrap_or(0).min(34);
    for diff in diffs {
        out.push_str(&format!(
            "  {} {:<width$}  {}  ->  {}\n",
            if diff.invalidates { "⛔" } else { "⚠️ " },
            diff.field,
            diff.baseline,
            diff.current,
            width = width
        ));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(machine: &str, memory: &str) -> RunIdentity {
        RunIdentity {
            machine_id: machine.to_string(),
            memory_id: memory.to_string(),
            simd_capabilities: "AVX2".to_string(),
            tsc_frequency_ghz: 3.0,
            ..Default::default()
        }
    }

    #[test]
    fn identical_identities_produce_no_differences() {
        let a = identity("abc", "def");
        let b = identity("abc", "def");
        assert!(compare_identity(&a, &b).is_empty());
    }

    #[test]
    fn machine_and_memory_changes_invalidate_separately() {
        let base = identity("abc", "def");

        let cpu_swapped = identity("zzz", "def");
        let diffs = compare_identity(&base, &cpu_swapped);
        assert!(diffs.iter().any(|d| d.field.starts_with("Machine ID") && d.invalidates));
        assert!(!diffs.iter().any(|d| d.field.starts_with("Memory fingerprint")));

        let ram_swapped = identity("abc", "zzz");
        let diffs = compare_identity(&base, &ram_swapped);
        assert!(diffs.iter().any(|d| d.field.starts_with("Memory fingerprint") && d.invalidates));
        assert!(!diffs.iter().any(|d| d.field.starts_with("Machine ID")));
    }

    #[test]
    fn size_tolerance_absorbs_rounding_but_not_real_change() {
        // The bug case: 7 GiB vs 3 GiB per thread must be flagged.
        assert!(differs_materially(7.0, 3.0));
        // Page/block rounding on the same spec must not be.
        assert!(!differs_materially(3.000, 3.002));
        assert!(!differs_materially(0.0, 0.0));
    }

    fn config_with(per_thread_gib: f64, threads: usize) -> RunConfigSnapshot {
        RunConfigSnapshot {
            memory: MemorySnapshot {
                allocated_gib: per_thread_gib * threads as f64,
                per_thread_gib,
                spec: "20%-from-available".to_string(),
                spec_is_deterministic: true,
                ..Default::default()
            },
            threads: ThreadSnapshot { thread_count: threads, active_threads_per_core: 1, ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn differing_working_set_invalidates_the_comparison() {
        // Exactly the 2026-07-31 vs 2026-09-08 case that produced confident nonsense.
        let baseline = config_with(7.0, 8);
        let current = config_with(3.0, 8);
        let diffs = compare_config(&baseline, &current);
        assert!(diffs.iter().any(|d| d.field == "Per-thread allocation" && d.invalidates));
        assert!(diffs.iter().any(|d| d.field == "Total allocation" && d.invalidates));
        assert!(render_differences(&diffs).contains("NOT A LIKE-FOR-LIKE"));
    }

    #[test]
    fn identical_configs_produce_no_differences() {
        let a = config_with(3.0, 8);
        let b = config_with(3.0, 8);
        assert!(compare_config(&a, &b).is_empty());
        assert!(render_differences(&[]).is_empty());
    }

    #[test]
    fn non_deterministic_spec_is_flagged_even_when_sizes_match() {
        let mut a = config_with(3.0, 8);
        let mut b = config_with(3.0, 8);
        a.memory.spec_is_deterministic = false;
        b.memory.spec_is_deterministic = false;
        let diffs = compare_config(&a, &b);
        assert!(diffs.iter().any(|d| d.field == "Memory spec reproducibility"));
        // A warning, not a disqualification — the sizes did match this time.
        assert!(!diffs.iter().any(|d| d.invalidates));
    }

    #[test]
    fn page_mix_describes_every_kind_used() {
        let mut mix = PageSizeMix { huge_1gib_gib: 1.0, ..Default::default() };
        assert_eq!(mix.describe(), "1.00 GiB 1 GiB pages");

        mix.large_2mib_gib = 54.0;
        assert_eq!(mix.describe(), "1.00 GiB 1 GiB pages, 54.00 GiB 2 MiB pages");

        assert_eq!(PageSizeMix::default().describe(), "none");
    }

    /// TODO 99: the results file records the seal's kernel, as the plan names it, and where its width came from, in a sealed run; neither
    /// in an unsealed one, or in results written before they were recorded.
    #[test]
    fn the_run_records_its_seal_and_where_its_width_came_from() {
        use crate::seal::{SealKernel, SealPattern, SealWidth, SealWidthSource};
        let capture =
            |seal| ExecutionSnapshot::capture(&crate::runner::TestSuiteTiming::cycles_only(1), crate::ErrorMode::Log, None, None, 2, seal);
        for (pattern, width, source, recorded) in [
            (SealPattern::Tmr, SealWidth::W128, SealWidthSource::Calibrated, ("tmr-128", "calibrated")),
            (SealPattern::Tm5, SealWidth::W256, SealWidthSource::Rule, ("tm5-256", "rule")),
            (
                SealPattern::Tmr,
                SealWidth::W512,
                SealWidthSource::Setting("the config's seal_width"),
                ("tmr-512", "the config's seal_width"),
            ),
        ] {
            let execution = capture(Some((SealKernel::new(pattern, width), source)));
            assert_eq!((execution.seal.as_deref(), execution.seal_width_source.as_deref()), (Some(recorded.0), Some(recorded.1)));
        }
        let unsealed = capture(None);
        assert_eq!((unsealed.seal.as_deref(), unsealed.seal_width_source.as_deref()), (None, None));

        let mut json =
            serde_json::to_value(capture(Some((SealKernel::new(SealPattern::Tmr, SealWidth::W256), SealWidthSource::Rule)))).unwrap();
        for key in ["seal", "seal_width_source"] {
            json.as_object_mut().unwrap().remove(key);
        }
        let older: ExecutionSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!((older.seal, older.seal_width_source), (None, None));
    }

    /// TODO 99: the results file records the run's width and where it came from, under auto too (the widest the CPU has), and each test
    /// that takes the run's width the width it ran at: an `-Auto` test its width, Mem-Sequence 256 for 128, a Bench pair naming none the
    /// run's. A test named for a width, or with none, records none.
    #[test]
    fn the_run_and_each_test_record_their_width() {
        use crate::simd::{TestWidth, Width};
        let cache_info = crate::tests::get_cache_info();
        let forced = [Width::W128, Width::W256, Width::W512].into_iter().filter(|w| w.check().is_ok()).map(|w| (w, Some(w)));
        for (width, given) in forced.chain([(Width::widest(), None)]) {
            let _width = TestWidth::set(given);
            let execution =
                ExecutionSnapshot::capture(&crate::runner::TestSuiteTiming::cycles_only(1), crate::ErrorMode::Log, None, None, 2, None);
            let source = if given.is_some() { "a test's width" } else { "auto" };
            assert_eq!((execution.simd_width_bits, execution.simd_width_source.as_deref()), (Some(width.bits()), Some(source)));
            let definitions = crate::runner::create_test_definitions(cache_info);
            let recorded = |name: &str| {
                let def = definitions.iter().find(|d| d.display_name == name).unwrap_or_else(|| panic!("no {name}"));
                TestConfigSnapshot::capture(def, cache_info, 1, 64 << 20).simd_width_bits
            };
            let sequence = width.max(Width::W256).bits();
            for (name, bits) in [
                (format!("Mem-StuckBit{}_A", width.name()), Some(width.bits())),
                ("Mem-Sequence".to_string(), Some(sequence)),
                ("Bench-Init-TMR-10".to_string(), Some(width.bits())),
                ("Bench-Verify-Random".to_string(), Some(width.bits())),
                ("Mem-StuckBit128".to_string(), None),
                ("Mem-MovingInv".to_string(), None),
            ] {
                assert_eq!(recorded(&name), bits, "{name} under {given:?}");
            }
        }
    }

    /// TODO 99: a test that ran at another width under the same name (Mem-Sequence, a Bench pair) isn't like for like; one from results
    /// written before the width was recorded isn't flagged.
    #[test]
    fn a_test_at_another_width_invalidates_the_comparison() {
        let with_test = |width: Option<u32>| {
            let mut config = config_with(3.0, 8);
            config.tests.push(TestConfigSnapshot { name: "Mem-Sequence".to_string(), simd_width_bits: width, ..Default::default() });
            config
        };
        let diffs = compare_config(&with_test(Some(512)), &with_test(Some(256)));
        assert!(
            diffs
                .iter()
                .any(|d| d.field == "Mem-Sequence: SIMD width" && d.invalidates && d.baseline == "512 bits" && d.current == "256 bits"),
            "{diffs:?}"
        );
        assert!(compare_config(&with_test(Some(256)), &with_test(Some(256))).is_empty());
        assert!(compare_config(&with_test(None), &with_test(Some(512))).is_empty());
    }

    #[test]
    fn mixed_pages_are_not_reported_as_invalidating() {
        // Page mix varies with host fragmentation, not with the command line, so it must warn rather than disqualify — otherwise every
        // long-uptime run "invalidates" the last one.
        let mut a = config_with(3.0, 8);
        let mut b = config_with(3.0, 8);
        a.memory.page_mix.huge_1gib_gib = 8.0;
        b.memory.page_mix.large_2mib_gib = 8.0;
        let diffs = compare_config(&a, &b);
        assert!(diffs.iter().any(|d| d.field == "Page-size mix"));
        assert!(!diffs.iter().any(|d| d.invalidates));
    }
}
