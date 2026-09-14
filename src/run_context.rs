//! Run identity and resolved configuration, recorded into every result file (TODO #67 items 1/2).
//!
//! # Why this exists
//!
//! A result file used to hold nothing but timings, which made `--compare-results` confidently
//! wrong. A 2026-07-31 baseline compared against a 2026-09-08 run reported StuckBit "+10-20%
//! faster" and Refresh "-14% slower" with no code change between them: the baseline had run
//! **8 threads × 7 GiB** and the new run **8 threads × 3 GiB**. The smaller working set is more
//! cache-resident (StuckBit up) while Refresh's fixed 64 ms sleeps became a larger share of a
//! shorter run (Refresh down). None of that was in either result file — it had to be
//! reconstructed by hand from `logs/`, and the comparison had no basis on which to warn.
//!
//! So: every result now carries **what machine ran it** ([`RunIdentity`]) and **what was actually
//! allocated and executed** ([`RunConfigSnapshot`]). Both are captured once per run, off any test
//! path.
//!
//! # Request vs. resolved
//!
//! The distinction runs through this whole module and is the point of it. A *request* (`memory=60%`)
//! and the *outcome* (`8 × 3.000 GiB on 2 MB pages`) are different facts, and only the outcome
//! explains a throughput number. Where they can diverge, both are stored.

use std::sync::OnceLock;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::app_config::MachineIdentity;
use crate::constants::bytes_to_gib_f64 as to_gib;
use crate::formatting::{ByteSize, serialize_round_2dp, serialize_round_opt_2dp};
use crate::smbios::{MemoryModule, SmbiosData};

// ===========================================================================
// Run instant — the one clock reading that names this run
// ===========================================================================

/// The single wall-clock instant identifying this run, captured on first call.
///
/// **Everything that names or stamps a run must read it from here.** The log filename
/// (`main.rs: setup_logging`) and the result filename (`results.rs: TestRunResult::new`) used to
/// call `Local::now()` and `Utc::now()` *independently*, and not at the same moment: logging is
/// initialised near the top of `main`, while the result is built in `runner.rs` after allocation
/// and `ThreadPool::new`. So the two artifacts of one run disagreed twice over —
/// `results/TMR_2026-09-08_04-18-35.json` next to `logs/TMR_2026-09-08_14-18-23.log`, a 10 h zone
/// difference *and* a 12 s difference — and could not be paired by name at all. Fixing only the
/// timezone would have left the seconds skewed, which is why this is a shared instant rather than
/// two matching format strings.
///
/// Local, not UTC, because it is the stem of a filename a human browses. Result *files* carry both
/// zones as fields (see [`crate::results::TestRunMetadata`]); a filename can only pick one.
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

/// Which machine produced a result, in enough detail to tell "the code got faster" from
/// "the hardware changed".
///
/// Nothing here is newly detected: [`crate::app_config::AppConfig::generate_machine_id`] and
/// [`SmbiosData::memory_fingerprint`] already run on **every** invocation (via
/// `is_calibration_valid`, which is why TMR can print `Calibration: Stale (hardware changed)`).
/// This type just hands the same facts to the result file, which never received them before.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunIdentity {
    /// Hash of CPU vendor/brand/family/model/stepping + microcode + cache sizes + core count +
    /// base frequency + BIOS vendor/version/date. Same value that gates calibration reuse, so a
    /// mismatch here means calibration would also have been rejected.
    pub machine_id: String,

    /// Hash of the SMBIOS memory configuration. Tracked separately from `machine_id` because RAM
    /// can be swapped or retimed without touching the CPU — and that is exactly the change most
    /// likely to move a memory test's numbers.
    pub memory_id: String,

    /// SMBIOS Type 1 system UUID. Distinguishes a VM migration / re-launch from a hardware change
    /// when `machine_id` still matches.
    pub system_uuid: Option<String>,

    /// Readable counterpart to `machine_id` — when the hash mismatches, these fields say what
    /// actually differs instead of leaving two opaque hex strings.
    pub machine: MachineIdentity,

    pub physical_cores: usize,
    pub logical_cores: usize,
    /// Whether the CPU exposes SMT siblings at all. How many TMR *used* is in
    /// [`ThreadSnapshot::active_threads_per_core`] — a machine capability and a run choice.
    pub smt_available: bool,

    /// Cache geometry. Recorded because cache-relative window and chunk modes are computed from
    /// these numbers, so two machines with the same `machine_id`-relevant CPU but different L3
    /// (a different SKU, or a hypervisor presenting a different slice) will size tests differently.
    pub l1d_cache_per_core: ByteSize,
    pub l2_cache_per_core: ByteSize,
    pub l3_cache: ByteSize,
    /// Left as a plain number: 64 needs no scaling, and it is used as a stride/divisor rather than
    /// read as a capacity.
    pub cache_line_bytes: usize,
    /// How the cache sizes above were obtained (CPUID leaf, Windows API, or fallback defaults).
    /// A result sized off "Default fallback values" is not comparable with a properly detected one.
    pub cache_detection_method: String,

    /// TSC frequency in GHz. Every latency figure is derived from this, so a drift here rescales
    /// them all — which is why calibration already treats a change as invalidating.
    pub tsc_frequency_ghz: f64,

    /// Physically installed RAM (`GetPhysicallyInstalledSystemMemory`).
    #[serde(serialize_with = "serialize_round_2dp")]
    pub installed_memory_gib: f64,
    /// RAM the OS reports as usable (`GlobalMemoryStatusEx`). Lower than installed on machines
    /// that carve out firmware/graphics reservations.
    #[serde(serialize_with = "serialize_round_2dp")]
    pub os_visible_memory_gib: f64,

    /// SIMD instruction sets available at runtime. Auto-dispatch tests resolve their width from
    /// this, so it determines which code path a `*Auto` test actually ran.
    pub simd_capabilities: String,

    /// Running under a hypervisor (CPUID hypervisor-present bit), and its vendor if it identified
    /// itself. Relevant beyond bookkeeping: a hypervisor can hide NUMA nodes, present a partial
    /// L3, and make physical page availability (hence page-size mix) vary run to run.
    pub virtualized: bool,
    pub hypervisor: Option<String>,

    /// Number of memory modules SMBIOS reported, and their summed capacity.
    pub memory_modules_reported: usize,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub memory_reported_gib: f64,

    /// Per-module SMBIOS Type 17 detail.
    ///
    /// **Read `memory_identity_is_physical` before trusting this as DIMM identity.** Stored in full
    /// rather than hashed away, because on bare metal it is the single most useful thing to have
    /// stamped on a memory-test result (part number, speed, serial). On a VM it is firmware
    /// fiction: this EC2 box reports `1 module, 64.0 GB` for what is really multi-channel host
    /// DRAM, with no serial. Keeping the raw report and labelling it beats silently dropping it.
    pub memory_modules: Vec<MemoryModule>,

    /// Whether `memory_modules` describes real DIMMs (bare metal) or a synthetic firmware view
    /// (virtualized). `memory_id` stays useful either way — on a VM it changes when the instance
    /// is retyped, which is the signal worth having.
    pub memory_identity_is_physical: bool,
}

impl RunIdentity {
    /// Snapshot the current machine.
    ///
    /// Called from `TestRunResult::new()` rather than exposed as an optional setter, deliberately:
    /// the bug this fixes was a `SystemInfoSnapshot` full of `"Unknown"` / `0.0` placeholders that
    /// no code ever populated. Wiring it into construction means it cannot be forgotten again.
    ///
    /// Cost is a few Win32 calls and one SMBIOS firmware read, once per run, before any test
    /// thread exists. CPU/cache data comes from the process-wide cached `SystemInfo`.
    pub fn detect() -> Self {
        let sys = crate::tests::get_system_info();
        let cache = sys.get_cache_info();
        let smbios = SmbiosData::detect();

        // Installed vs OS-visible RAM. Failure is not worth aborting a run over — leave zeros,
        // which read as "not recorded" next to a populated `machine_id`.
        let (installed_memory_gib, os_visible_memory_gib) =
            match crate::memory::allocation_strategy::SystemMemoryInfo::gather() {
                Ok(info) => (
                    to_gib(info.total_installed_bytes),
                    to_gib(info.total_physical_bytes),
                ),
                Err(e) => {
                    log::warn!("Could not record system memory totals in result: {}", e);
                    (0.0, 0.0)
                }
            };

        let virtualized = cache.is_virtual_machine;

        Self {
            machine_id: crate::app_config::AppConfig::generate_machine_id(sys, &smbios),
            memory_id: smbios.memory_fingerprint(),
            system_uuid: if smbios.system.uuid.is_empty() {
                None
            } else {
                Some(smbios.system.uuid.clone())
            },
            // Same constructor `AppConfig::update_identity` uses, so a result file's readable
            // identity always corresponds to the `machine_id` that gates calibration reuse.
            machine: MachineIdentity::detect(sys, &smbios),
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

/// What was asked for and what the run actually got. Captured once, after allocation and thread
/// creation, so the resolved figures are measured rather than predicted.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunConfigSnapshot {
    /// `argv`, with the executable path reduced to its filename. The canonical `memory`/`threads`
    /// fields below are TMR's *interpretation*; this is the record of what was typed, and it also
    /// captures flags with no structured home (`--quick-test`, `cpu-stride=`, `skip-cores=`).
    ///
    /// `argv[0]` is stripped to the bare filename because result files get shared and the install
    /// directory is both useless to a reader and a way to leak a username. The filename itself is
    /// kept — it can carry a build tag (`tmr-avx512.exe`) that explains a result.
    pub command_line: Vec<String>,

    /// Config name from the loaded config's metadata, if the run was driven by one (JSON or TM5
    /// `.cfg`). The *path* is in `command_line`; this is the config's own declared name, which is
    /// what community configs are known by (`"1usmus_v3"`).
    pub config_name: Option<String>,

    pub memory: MemorySnapshot,
    pub threads: ThreadSnapshot,
    pub execution: ExecutionSnapshot,

    /// Calibration that was live during the run. `None` means cache-relative windows fell back to
    /// CPUID cache sizes — a materially different sizing basis, so worth distinguishing.
    pub calibration: Option<CalibrationSnapshot>,

    /// One entry per test in the resolved plan. Stored at run level rather than repeated inside
    /// every cycle's results: this is configuration, and it does not change between cycles.
    pub tests: Vec<TestConfigSnapshot>,
}

/// Memory request, and the allocation that came back.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemorySnapshot {
    /// Canonical `memory=` spec reconstructed from the parsed strategy, e.g.
    /// `"20%-from-available:start=split:5%:95%"`.
    pub spec: String,

    /// Whether `spec` yields the same size on every run of this machine.
    ///
    /// **`false` is the trap that motivated this whole module.** `-from-available` sizes off
    /// *currently free* memory: two `--quick-test` runs minutes apart computed `12.414 GiB` and
    /// `11.996 GiB` raw and only agreed on 3.000 GiB/thread after rounding. That is luck, not
    /// reproducibility. Only `-from-total` and `-target` are deterministic.
    pub spec_is_deterministic: bool,

    /// Human-readable strategy label from the allocator (e.g. `"Reserve from Available (5%:95%)"`).
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
    /// Split-reserve breakdown, when the start-address mode splits the reserve either side of the
    /// test region.
    #[serde(serialize_with = "serialize_round_opt_2dp")]
    pub reserve_pre_gib: Option<f64>,
    #[serde(serialize_with = "serialize_round_opt_2dp")]
    pub reserve_post_gib: Option<f64>,
    /// Lowest address the allocator was told to accept.
    pub min_start_address: ByteSize,

    /// Free memory and load at the moment the plan was computed. For a non-deterministic spec this
    /// is the input that decided the size, so it is the only way to explain why two runs differ.
    #[serde(serialize_with = "serialize_round_2dp")]
    pub available_at_start_gib: f64,
    pub memory_load_percent_at_start: u32,

    /// What actually came back, summed over every allocated block. May be below `requested_gib`:
    /// a partially satisfied run still produces results (one observed run got 7 of 8 blocks).
    #[serde(serialize_with = "serialize_round_2dp")]
    pub allocated_gib: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub per_thread_gib: f64,
    pub blocks_allocated: usize,

    pub backend: String,
    pub large_pages_available: bool,
    pub min_page_size: String,
    pub max_page_size: String,

    pub page_mix: PageSizeMix,
}

/// How allocated bytes split across page sizes.
///
/// Not cosmetic, and not stable between runs: the allocator takes 1 GB huge pages when physical
/// memory is contiguous enough and falls back to 2 MB otherwise, so the same request lands
/// differently as fragmentation changes. One observed pair of same-size runs differed 22 GiB vs
/// 1 GiB on huge pages, purely from uptime. TLB reach differs sharply between the two, so **two
/// runs with identical totals are not automatically comparable** — this is what says so.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PageSizeMix {
    pub huge_1gib_blocks: usize,
    pub large_2mib_blocks: usize,
    pub regular_4kib_blocks: usize,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub huge_1gib_gib: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub large_2mib_gib: f64,
    #[serde(serialize_with = "serialize_round_2dp")]
    pub regular_4kib_gib: f64,
}

impl PageSizeMix {
    /// Tally page types across every allocated block.
    ///
    /// Takes the allocator's per-thread map and flattens it: the mix is a whole-run property
    /// because it is driven by physical contiguity, not by thread.
    ///
    /// Must be called **before** the blocks are moved into `ThreadPool::new` — after that the
    /// runner no longer owns them.
    pub fn from_blocks(
        thread_blocks: &std::collections::HashMap<usize, Vec<crate::runner::AllocationBlock>>,
    ) -> Self {
        let mut mix = Self::default();
        for blocks in thread_blocks.values() {
            for block in blocks {
                let bytes = block.buffer.size() as u64;
                if block.buffer.uses_huge_pages() {
                    mix.huge_1gib_blocks += 1;
                    mix.huge_1gib_gib += to_gib(bytes);
                } else if block.buffer.uses_large_pages() {
                    mix.large_2mib_blocks += 1;
                    mix.large_2mib_gib += to_gib(bytes);
                } else {
                    mix.regular_4kib_blocks += 1;
                    mix.regular_4kib_gib += to_gib(bytes);
                }
            }
        }
        mix
    }

    /// Compact one-line summary for console/report use, e.g. `"1 × 1GB huge, 27 × 2MB large"`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.huge_1gib_blocks > 0 {
            parts.push(format!(
                "{} block(s) 1GB huge ({:.2} GiB)",
                self.huge_1gib_blocks, self.huge_1gib_gib
            ));
        }
        if self.large_2mib_blocks > 0 {
            parts.push(format!(
                "{} block(s) 2MB large ({:.2} GiB)",
                self.large_2mib_blocks, self.large_2mib_gib
            ));
        }
        if self.regular_4kib_blocks > 0 {
            parts.push(format!(
                "{} block(s) 4KB regular ({:.2} GiB)",
                self.regular_4kib_blocks, self.regular_4kib_gib
            ));
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// Whether more than one page size was used. A mixed run's TLB behaviour is not uniform across
    /// threads, which is enough to make a per-thread throughput spread expected rather than
    /// suspicious.
    pub fn is_mixed(&self) -> bool {
        let kinds = [
            self.huge_1gib_blocks,
            self.large_2mib_blocks,
            self.regular_4kib_blocks,
        ];
        kinds.iter().filter(|n| **n > 0).count() > 1
    }
}

/// Worker-thread placement. Recorded per thread because *which* CPUs ran matters as much as how
/// many: on a multi-CCD part, packed versus spread placement changes throughput by 13-25%.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThreadSnapshot {
    pub thread_count: usize,
    /// Whether workers were pinned to specific logical CPUs. Unpinned, the OS scheduler may move
    /// a thread across cache or NUMA domains mid-test and the numbers stop meaning much.
    pub pinned: bool,
    /// `(thread_id, logical_cpu, numa_node)` as actually assigned.
    pub assignments: Vec<ThreadAssignment>,
    /// Distinct NUMA nodes used, and the thread count on each.
    pub numa_distribution: Vec<NumaThreadCount>,
    /// SMT siblings TMR treated as active (1 or 2). This is the **divisor** for L1/L2-relative
    /// window and chunk sizes, so the same `Cache (L2)` spec resolves to half the bytes at 2 —
    /// making it a direct input to what every cache-tier test measured.
    pub active_threads_per_core: usize,
    /// Resolved logical-CPU pool the pool drew from (`cpus=` / `skip-cores=` / `cpu-stride=`
    /// already applied). `None` means no restriction was given.
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
    /// Cycles requested. The count actually completed is `overall_stats.cycles_completed`, which
    /// is lower on an interrupted run — comparing an interrupted run to a complete one is a
    /// mistake worth being able to detect.
    pub cycles_requested: Option<u32>,
    pub duration_secs_requested: Option<u32>,
    pub per_test_cycle_multiplier: f64,
    pub error_mode: String,
    /// Glob filter that selected the plan (`test=Mem-StuckBit*`), if any.
    pub test_filter: Option<String>,
    /// Configured memory channels. Feeds SimpleTest's stride formula, so it changes the access
    /// pattern rather than just labelling it.
    pub channels: u32,
}

/// The calibration that was live during the run. Cache-relative windows are sized from these
/// measured tier boundaries, so a result is only comparable with another sized off the same ones.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CalibrationSnapshot {
    /// When the calibration was measured (ISO 8601 UTC).
    pub timestamp: String,
    /// Page size the calibration allocation used — a calibration run on 4 KB pages describes
    /// different tier boundaries than one on 2 MB.
    pub page_size: String,
    /// Measured optimal working-set size per tier, which is literally the number window sizing
    /// multiplies by the test's scale factor.
    pub tiers: Vec<CalibratedTier>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CalibratedTier {
    pub tier: String,
    pub optimal_size: ByteSize,
    /// Median latency measured at `optimal_size`. Recorded alongside the size because it is the
    /// evidence for it — a tier whose latency looks wrong explains a window size that looks wrong.
    #[serde(serialize_with = "serialize_round_2dp")]
    pub median_latency_ns: f64,
}

/// Per-test configuration as resolved for this run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestConfigSnapshot {
    /// Registered/display name, with auto-dispatch already resolved (`Mem-StuckBit512_A`), so it
    /// matches the name results are keyed by.
    pub name: String,
    /// Underlying test function's own name. Several registrations share one function (the flush
    /// and `_A` variants), so this is not unique — it says which code path ran.
    pub function: String,
    /// Stage-2 per-test working set.
    pub window_mode: String,
    /// Stage-3 iteration unit within the window.
    pub chunk_mode: String,
    pub verify_reps: u32,
    pub test_reps: u32,
    pub write_read_cycles: u32,
    pub pattern_mode: Option<u32>,
    /// Whether the verify was forced through DRAM with CLFLUSHOPT. Costs real bandwidth by design,
    /// so a flushed and an unflushed run of the same test are not comparable.
    pub flush_before_verify: bool,
    /// Whether this test relied on a previous test's pattern instead of writing its own.
    pub skip_init: bool,
}

// ===========================================================================
// Capture
// ===========================================================================

/// Everything the runner already knows at capture time, bundled so the signature stays readable
/// (and clippy's `too_many_arguments` stays quiet without an `allow`).
pub struct RunConfigInputs<'a> {
    /// `layout.system_memory_info` — free memory and load as observed when the plan was computed.
    pub system_memory_info: &'a crate::memory::allocation_strategy::SystemMemoryInfo,
    /// `layout.allocation_result` — the *requested* figures.
    pub allocation_result: &'a crate::memory::allocation_strategy::AllocationResult,
    /// `layout.strategy` — the parsed `memory=` spec.
    pub strategy: &'a crate::memory::allocation_strategy::EnhancedMemoryStrategy,
    pub runtime_config: &'a crate::RuntimeConfig,
    /// Tallied from the blocks that actually came back, so it reflects the outcome.
    pub page_mix: PageSizeMix,
    pub allocated_bytes: u64,
    pub thread_count: usize,
    pub cpu_assignments: &'a [crate::thread_pool::CpuAssignment],
    pub pinned: bool,
    pub suite_timing: &'a crate::runner::TestSuiteTiming,
    pub error_mode: crate::ErrorMode,
    pub test_filter: Option<String>,
    pub channels: u32,
    pub config_name: Option<String>,
    /// The resolved plan — after filtering, overrides, and auto-dispatch.
    pub tests: &'a [crate::runner::TestDefinition],
    pub cache_info: &'a crate::cache::CacheInfo,
}

impl RunConfigSnapshot {
    /// Build the snapshot. Call after allocation and thread-pool creation so the resolved figures
    /// are observed rather than predicted, and before the first cycle starts.
    pub fn capture(inputs: RunConfigInputs<'_>) -> Self {
        let RunConfigInputs {
            system_memory_info,
            allocation_result: allocation,
            strategy,
            runtime_config,
            page_mix,
            allocated_bytes,
            thread_count,
            cpu_assignments,
            pinned,
            suite_timing,
            error_mode,
            test_filter,
            channels,
            config_name,
            tests,
            cache_info,
        } = inputs;

        let (reserve_pre_gib, reserve_post_gib) = match &allocation.split_details {
            Some(split) => (
                Some(to_gib(split.pre_buffer_bytes)),
                Some(to_gib(split.post_reserve_bytes)),
            ),
            None => (None, None),
        };

        let memory = MemorySnapshot {
            spec: strategy.describe_spec(),
            spec_is_deterministic: strategy.allocation_mode.is_deterministic(),
            allocation_type: allocation.allocation_type.clone(),
            reference_gib: to_gib(allocation.reference_bytes),
            requested_gib: to_gib(allocation.allocation_bytes),
            reserve_gib: to_gib(allocation.reserve_bytes),
            reserve_pre_gib,
            reserve_post_gib,
            min_start_address: allocation.min_start_address.into(),
            available_at_start_gib: to_gib(system_memory_info.available_physical_bytes),
            memory_load_percent_at_start: system_memory_info.memory_load_percent,
            allocated_gib: to_gib(allocated_bytes),
            per_thread_gib: if thread_count > 0 {
                to_gib(allocated_bytes / thread_count as u64)
            } else {
                0.0
            },
            blocks_allocated: page_mix.huge_1gib_blocks
                + page_mix.large_2mib_blocks
                + page_mix.regular_4kib_blocks,
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
            assignments: cpu_assignments
                .iter()
                .map(|&(thread_id, logical_cpu, numa_node)| ThreadAssignment {
                    thread_id,
                    logical_cpu,
                    numa_node,
                })
                .collect(),
            numa_distribution,
            active_threads_per_core: crate::tests::get_active_threads_per_core(),
            cpu_pool: runtime_config.cpu_list.clone(),
        };

        let execution = ExecutionSnapshot {
            cycles_requested: suite_timing.global_cycles,
            duration_secs_requested: suite_timing.global_duration_secs,
            per_test_cycle_multiplier: suite_timing.per_test_cycle_multiplier,
            error_mode: format!("{:?}", error_mode),
            test_filter,
            channels,
        };

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
            CalibrationSnapshot {
                timestamp: cal.timestamp.to_rfc3339(),
                page_size: cal.page_size.clone(),
                tiers,
            }
        });

        // Window/chunk modes are rendered with the same formatter the console tables use, so the
        // recorded string matches what the user saw — including the resolved byte size for cache
        // specs, which is where the SMT divisor becomes visible.
        let formatter = crate::reporting::formatters::DefaultFormatter::new();
        use crate::reporting::formatters::ReportFormatter;
        let tests = tests
            .iter()
            .map(|def| TestConfigSnapshot {
                name: def.display_name.clone(),
                function: def.actual_name.to_string(),
                window_mode: formatter.format_window_mode_with_size(
                    &def.config.window_mode,
                    cache_info,
                    thread_count,
                ),
                chunk_mode: formatter.format_chunk_mode_with_size(
                    &def.config.chunk_mode,
                    cache_info,
                    thread_count,
                ),
                verify_reps: def.config.verify_reps,
                test_reps: def.config.test_reps,
                write_read_cycles: def.config.write_read_cycles,
                pattern_mode: def.config.pattern_mode,
                flush_before_verify: def.config.flush_before_verify,
                skip_init: def.config.skip_init,
            })
            .collect();

        Self {
            command_line: capture_command_line(),
            config_name,
            memory,
            threads,
            execution,
            calibration,
            tests,
        }
    }
}

/// `argv` with the executable path reduced to its filename.
///
/// Only `argv[0]` is rewritten. Later arguments are left verbatim — they are what the user typed,
/// and `config=` in particular needs its path to stay meaningful. A caller who passes an absolute
/// config path still puts a directory in the record; that is their own text, not something TMR
/// injected on their behalf.
fn capture_command_line() -> Vec<String> {
    let mut args: Vec<String> = std::env::args().collect();
    if let Some(exe) = args.first_mut()
        && let Some(name) = std::path::Path::new(exe.as_str())
            .file_name()
            .and_then(|n| n.to_str())
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
    /// `true` when the difference alone is enough to invalidate the throughput comparison, as
    /// opposed to being worth noting. Drives ⛔ vs ⚠️ in the report.
    pub invalidates: bool,
}

impl RunDifference {
    fn new(field: &str, baseline: impl Into<String>, current: impl Into<String>, invalidates: bool) -> Self {
        Self {
            field: field.to_string(),
            baseline: baseline.into(),
            current: current.into(),
            invalidates,
        }
    }
}

/// Percentage difference that counts as a real size change rather than rounding.
///
/// Allocation sizes are rounded to page and block boundaries, so two runs of an identical spec can
/// land a hair apart. 1% is well below any threshold at which working-set size moves throughput,
/// and well above that rounding.
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
/// Hardware differences are reported as *invalidating*: if the CPU, RAM, or cache geometry changed
/// then a throughput delta says nothing about the code, which is exactly the mistake that
/// motivated TODO #67.
pub fn compare_identity(baseline: &RunIdentity, current: &RunIdentity) -> Vec<RunDifference> {
    let mut diffs = Vec::new();

    if baseline.machine_id != current.machine_id {
        diffs.push(RunDifference::new(
            "Machine ID (CPU/cache/BIOS)",
            &baseline.machine_id,
            &current.machine_id,
            true,
        ));
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
            diffs.push(RunDifference::new(
                "  Physical cores",
                m.0.physical_cores.to_string(),
                m.1.physical_cores.to_string(),
                true,
            ));
        }
    }

    if baseline.memory_id != current.memory_id {
        // Deliberately separate from machine_id: RAM can change with the CPU untouched, and for a
        // memory tester that is the more consequential half.
        diffs.push(RunDifference::new(
            "Memory fingerprint",
            &baseline.memory_id,
            &current.memory_id,
            true,
        ));
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
        // Same hardware model, different machine or VM instance. Not invalidating by itself —
        // two identical hosts are a fair comparison — but it explains run-to-run noise.
        diffs.push(RunDifference::new(
            "System UUID",
            baseline.system_uuid.clone().unwrap_or_else(|| "none".into()),
            current.system_uuid.clone().unwrap_or_else(|| "none".into()),
            false,
        ));
    }

    if baseline.l3_cache != current.l3_cache {
        diffs.push(RunDifference::new(
            "L3 cache",
            baseline.l3_cache.to_string(),
            current.l3_cache.to_string(),
            true,
        ));
    }

    if baseline.simd_capabilities != current.simd_capabilities {
        // Auto-dispatch resolves width from this, so a change can silently swap the code path
        // under a test whose name did not change.
        diffs.push(RunDifference::new(
            "SIMD capabilities",
            &baseline.simd_capabilities,
            &current.simd_capabilities,
            true,
        ));
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
        diffs.push(RunDifference::new(
            "Cache detection",
            &baseline.cache_detection_method,
            &current.cache_detection_method,
            false,
        ));
    }

    diffs
}

/// Compare the resolved configuration of two runs.
///
/// Working-set size and thread count dominate throughput, so a difference in either makes the
/// per-test percentages meaningless regardless of what the code did in between.
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
        // The one that actually bit us: same test, different per-thread working set, so cache
        // residency differs and the throughput delta is about size, not code.
        diffs.push(RunDifference::new(
            "Per-thread allocation",
            format!("{:.3} GiB", b.memory.per_thread_gib),
            format!("{:.3} GiB", c.memory.per_thread_gib),
            true,
        ));
    }

    if b.threads.thread_count != c.threads.thread_count {
        diffs.push(RunDifference::new(
            "Thread count",
            b.threads.thread_count.to_string(),
            c.threads.thread_count.to_string(),
            true,
        ));
    }

    if b.threads.active_threads_per_core != c.threads.active_threads_per_core {
        // Halves or doubles every L1/L2-relative window and chunk size.
        diffs.push(RunDifference::new(
            "Active threads per core (SMT)",
            b.threads.active_threads_per_core.to_string(),
            c.threads.active_threads_per_core.to_string(),
            true,
        ));
    }

    if b.threads.assignments != c.threads.assignments
        && b.threads.thread_count == c.threads.thread_count
    {
        let cpus = |s: &ThreadSnapshot| {
            s.assignments
                .iter()
                .map(|a| a.logical_cpu.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        // Same count, different placement. On a multi-CCD part this alone moves throughput
        // 13-25%, so it is worth flagging even though it is not a config error.
        diffs.push(RunDifference::new(
            "CPU placement",
            cpus(&b.threads),
            cpus(&c.threads),
            false,
        ));
    }

    if b.threads.numa_distribution != c.threads.numa_distribution {
        let nodes = |s: &ThreadSnapshot| {
            s.numa_distribution
                .iter()
                .map(|n| format!("node{}×{}", n.numa_node, n.threads))
                .collect::<Vec<_>>()
                .join(" ")
        };
        diffs.push(RunDifference::new(
            "NUMA distribution",
            nodes(&b.threads),
            nodes(&c.threads),
            false,
        ));
    }

    if b.threads.pinned != c.threads.pinned {
        diffs.push(RunDifference::new(
            "Thread pinning",
            if b.threads.pinned { "pinned" } else { "unpinned" },
            if c.threads.pinned { "pinned" } else { "unpinned" },
            true,
        ));
    }

    if b.memory.page_mix.huge_1gib_blocks != c.memory.page_mix.huge_1gib_blocks
        || b.memory.page_mix.large_2mib_blocks != c.memory.page_mix.large_2mib_blocks
        || b.memory.page_mix.regular_4kib_blocks != c.memory.page_mix.regular_4kib_blocks
    {
        // Not a config difference — an environment one. The allocator takes what physical
        // contiguity allows, so this varies with uptime even for an identical command line.
        diffs.push(RunDifference::new(
            "Page-size mix",
            b.memory.page_mix.describe(),
            c.memory.page_mix.describe(),
            false,
        ));
    }

    if b.memory.spec != c.memory.spec {
        diffs.push(RunDifference::new("Memory spec", &b.memory.spec, &c.memory.spec, false));
    }

    if !b.memory.spec_is_deterministic || !c.memory.spec_is_deterministic {
        // Even when the sizes happened to match, say so: it was not guaranteed to, so a future
        // re-run of the same command need not reproduce either number.
        diffs.push(RunDifference::new(
            "Memory spec reproducibility",
            if b.memory.spec_is_deterministic { "deterministic" } else { "depends on free memory" },
            if c.memory.spec_is_deterministic { "deterministic" } else { "depends on free memory" },
            false,
        ));
    }

    if b.execution.cycles_requested != c.execution.cycles_requested {
        let fmt = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_else(|| "unlimited".into());
        diffs.push(RunDifference::new(
            "Cycles requested",
            fmt(b.execution.cycles_requested),
            fmt(c.execution.cycles_requested),
            false,
        ));
    }

    if b.execution.channels != c.execution.channels {
        // Changes SimpleTest's stride, i.e. the access pattern itself.
        diffs.push(RunDifference::new(
            "Memory channels",
            b.execution.channels.to_string(),
            c.execution.channels.to_string(),
            true,
        ));
    }

    if b.memory.backend != c.memory.backend {
        diffs.push(RunDifference::new("Memory backend", &b.memory.backend, &c.memory.backend, true));
    }

    // Per-test config, for tests present in both plans. A window/chunk/flush change explains a
    // single test moving while its neighbours did not — otherwise easy to misread as a regression.
    for cur_test in &c.tests {
        let Some(base_test) = b.tests.iter().find(|t| t.name == cur_test.name) else {
            continue;
        };
        let mut note = |field: &str, bv: String, cv: String| {
            diffs.push(RunDifference::new(
                &format!("{}: {}", cur_test.name, field),
                bv,
                cv,
                true,
            ));
        };
        if base_test.window_mode != cur_test.window_mode {
            note("window", base_test.window_mode.clone(), cur_test.window_mode.clone());
        }
        if base_test.chunk_mode != cur_test.chunk_mode {
            note("chunk", base_test.chunk_mode.clone(), cur_test.chunk_mode.clone());
        }
        if base_test.flush_before_verify != cur_test.flush_before_verify {
            note(
                "flush before verify",
                base_test.flush_before_verify.to_string(),
                cur_test.flush_before_verify.to_string(),
            );
        }
        if base_test.verify_reps != cur_test.verify_reps {
            note("verify reps", base_test.verify_reps.to_string(), cur_test.verify_reps.to_string());
        }
        if base_test.write_read_cycles != cur_test.write_read_cycles {
            note(
                "write/read cycles",
                base_test.write_read_cycles.to_string(),
                cur_test.write_read_cycles.to_string(),
            );
        }
        if base_test.function != cur_test.function {
            // Same registered name, different implementation — e.g. auto-dispatch resolved to a
            // different SIMD width because the machine changed.
            note("implementation", base_test.function.clone(), cur_test.function.clone());
        }
    }

    diffs
}

/// Render identity + config differences as a block for the comparison report. Empty string when
/// the two runs are equivalent, so callers can append unconditionally.
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
                spec: "20%-from-available:start=split:5%:95%".to_string(),
                spec_is_deterministic: true,
                ..Default::default()
            },
            threads: ThreadSnapshot {
                thread_count: threads,
                active_threads_per_core: 1,
                ..Default::default()
            },
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
    fn page_mix_reports_kinds_and_mixing() {
        let mut mix = PageSizeMix {
            huge_1gib_blocks: 1,
            huge_1gib_gib: 1.0,
            ..Default::default()
        };
        assert!(!mix.is_mixed());
        assert!(mix.describe().contains("1GB huge"));

        mix.large_2mib_blocks = 27;
        mix.large_2mib_gib = 54.0;
        assert!(mix.is_mixed());
        let described = mix.describe();
        assert!(described.contains("1GB huge") && described.contains("2MB large"));

        assert_eq!(PageSizeMix::default().describe(), "none");
    }

    #[test]
    fn mixed_pages_are_not_reported_as_invalidating() {
        // Page mix varies with host fragmentation, not with the command line, so it must warn
        // rather than disqualify — otherwise every long-uptime run "invalidates" the last one.
        let mut a = config_with(3.0, 8);
        let mut b = config_with(3.0, 8);
        a.memory.page_mix.huge_1gib_blocks = 8;
        b.memory.page_mix.large_2mib_blocks = 8;
        let diffs = compare_config(&a, &b);
        assert!(diffs.iter().any(|d| d.field == "Page-size mix"));
        assert!(!diffs.iter().any(|d| d.invalidates));
    }
}
