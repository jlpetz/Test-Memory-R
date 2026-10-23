use std::collections::HashSet;

/// Simplified conversion functions that work with the actual codebase structure
/// This bridges the gap between the current codebase and the new reporting system
use super::models::*;
use crate::constants::{HUGE_PAGE_SIZE_USIZE, bytes_to_gib_f64};
use crate::memory::allocation_strategy::{AllocationResult, SystemMemoryInfo};
use crate::progress::TestSummary;

/// A test's parameters for the plan and report: the first one set, in the input's own form
pub(crate) fn format_parameter_context(ctx: &Option<crate::config::TestParameterContext>) -> String {
    match ctx {
        Some(c) => {
            if let Some(mirror) = c.mirror {
                return mirror.to_string();
            }
            if let Some(stride_cl) = c.stride_cachelines
                && stride_cl > 0
            {
                let stride_bytes = stride_cl * 64; // cache line = 64 bytes
                return format!("Stride({}cl/{}B)", stride_cl, stride_bytes);
            }
            // TMR-native test parameters
            if let Some(sp) = c.stride_patterns {
                return format!("StridePat({})", sp);
            }
            if let Some(rng) = c.rng_sequences {
                return format!("RngSeq({})", rng);
            }
            if let Some(sub) = c.subdivisions {
                return format!("Subdiv({})", sub);
            }
            if let Some(cd) = c.copy_directions {
                return format!("CopyDir({})", cd);
            }
            if let Some(ms) = c.wait_ms {
                return match ms {
                    ms if ms % 60_000 == 0 && ms > 0 => format!("Wait({}min)", ms / 60_000),
                    ms if ms % 1000 == 0 && ms > 0 => format!("Wait({}s)", ms / 1000),
                    ms => format!("Wait({ms}ms)"),
                };
            }
            "-".to_string()
        }
        None => "-".to_string(),
    }
}

/// Create a consolidated memory report combining system info and allocation planning
pub fn create_consolidated_memory_report(mem_info: &SystemMemoryInfo, allocation_result: &AllocationResult) -> ConsolidatedMemoryReport {
    let mut warnings = Vec::new();

    // Check for failure mode testing
    if allocation_result.allocation_type.contains("from Total") || allocation_result.allocation_type.contains("Target") {
        warnings.push("🧪 FAILURE MODE TESTING DETECTED - This configuration may intentionally cause allocation failures".to_string());
        warnings.push("Use only for development/testing purposes!".to_string());

        if allocation_result.allocation_bytes > mem_info.available_physical_bytes * 2 {
            warnings.push("🚨 EXTREME: Allocation >2x available memory - will likely fail".to_string());
        }
    }

    ConsolidatedMemoryReport {
        total_installed_bytes: mem_info.total_installed_bytes,
        total_physical_bytes: mem_info.total_physical_bytes,
        available_physical_bytes: mem_info.available_physical_bytes,
        used_physical_bytes: mem_info.used_physical_bytes,
        total_virtual_bytes: mem_info.total_virtual_bytes,
        memory_load_percent: mem_info.memory_load_percent,
        reference_bytes: allocation_result.reference_bytes,
        reference_name: allocation_result.reference_name,
        requested_reserve_bytes: allocation_result.requested_reserve_bytes,
        target_bytes: allocation_result.target_bytes,
        raw_allocation_bytes: allocation_result.raw_allocation_bytes,
        thread_count: allocation_result.thread_count,
        per_thread_raw_bytes: allocation_result.per_thread_raw_bytes,
        rounding_step_bytes: allocation_result.rounding_step_bytes,
        rounding_direction: allocation_result.rounding_direction.to_string(),
        rounding_asked: allocation_result.rounding_asked.map(|asked| (asked.step_bytes, asked.direction.to_string())),
        per_thread_bytes: allocation_result.per_thread_bytes,
        allocation_bytes: allocation_result.allocation_bytes,
        reserve_bytes: allocation_result.reserve_bytes,
        warnings,
    }
}

/// Convert TestDefinition structs to test configuration report (updated version)
/// Now accepts cache_info and thread_count to calculate actual extent sizes for cache targets
/// `thread_memory` is each thread's bytes (planned or allocated): extents and chunks are shown
/// resolved for the thread with the least, the range when threads differ. `tm5` is a TM5
/// import's `.cfg` window and lock granularity (MiB), for a header line.
/// What the plan shows of the cycle beyond its tests (TODO 74).
pub struct PlanSequence<'a> {
    /// `cycle_order` as written, each id with whether it runs; `None` without one
    pub order: Option<&'a [(String, bool)]>,
    /// The seal's kernel and where its width came from (TODO 99), when the run is sealed
    pub seal: Option<(crate::seal::SealKernel, crate::seal::SealWidthSource)>,
    /// Why it isn't, when it isn't
    pub seal_off_reason: &'a str,
    /// A TM5 import: tests go by the `.cfg`'s numbers
    pub tm5: bool,
    /// The SIMD width given and where (`width=`, a config's `width`, TODO 99); `None` under auto
    pub width: Option<(crate::simd::Width, &'a str)>,
}

/// The plan's note on a width given: what runs at it, and Mem-Sequence's where it lacks it.
fn width_note(width: crate::simd::Width, source: &str, test_definitions: &[crate::runner::TestDefinition]) -> String {
    let sequence = crate::tests::sequence_width(width);
    let lacks = if sequence != width && test_definitions.iter().any(|d| d.actual_name == "Mem-Sequence") {
        format!(" Mem-Sequence has no {}-bit kernel and runs at {}.", width.bits(), sequence.bits())
    } else {
        String::new()
    };
    format!(
        "ℹ  Width: {} bits ({source}) for every test that picks its width at run time: the -Auto tests (named \
             for it), Mem-Sequence and the Bench pairs; the seal too unless seal-width= sets its own. Tests named \
             for a width keep theirs.{lacks}",
        width.bits()
    )
}

/// The plan: one row per test, in the order of its first step, then the cycle's sequence when it
/// isn't simply each test once, and the seal (TODO 74).
pub fn create_test_configuration_report_v2(
    test_definitions: &[crate::runner::TestDefinition], suite_timing: &crate::runner::TestSuiteTiming,
    cache_info: &crate::cache::CacheInfo, thread_memory: &[usize], tm5: Option<(u32, u32)>, sequence: &PlanSequence,
) -> TestConfigurationReport {
    use crate::reporting::formatters::{DefaultFormatter, ReportFormatter};
    use crate::test_memory::{ChunkSpread, GRANULE};
    use crate::tests::{ChunkMode, ExtentMode};
    let formatter = DefaultFormatter::new();
    let size = |bytes: usize| formatter.format_bytes(bytes as u64);
    let thread_count = thread_memory.len();
    let least = thread_memory.iter().copied().min().unwrap_or(0);
    let most = thread_memory.iter().copied().max().unwrap_or(0);

    let mut notes = Vec::new();
    if let Some((window, lock)) = tm5 {
        notes.push(format!(
            "ℹ  TM5 import: every test covers the full allocation. The .cfg's {window} MiB Testing Window and \
                            {lock} MiB Lock Memory Granularity only size Test Block Size codes 0-3 and cap larger blocks."
        ));
    }

    let suite_timing_str = match (suite_timing.global_cycles, suite_timing.global_duration_secs) {
        (Some(cycles), Some(duration)) => format!("{} cycles or {}s max", cycles, duration),
        (Some(cycles), None) => format!("{} cycles", cycles),
        (None, Some(duration)) => format!("{}s duration", duration),
        (None, None) => "Unlimited".to_string(),
    };

    // Each test once, at its first step, with how many steps of the cycle run it
    let mut firsts: Vec<(&crate::runner::TestDefinition, usize)> = Vec::new();
    for def in test_definitions {
        match firsts.iter_mut().find(|(d, _)| d.test_index == def.test_index) {
            Some((_, steps)) => *steps += 1,
            None => firsts.push((def, 1)),
        }
    }
    let unique = firsts.len();

    let tests = firsts.iter().enumerate().map(|(i, &(test_def, steps))| {
        let config = &test_def.config;

        let timing = match (&config.timing.cycles, &config.timing.duration_secs) {
            (Some(c), Some(d)) => format!("{}cycles/{}s", c, d),
            (Some(c), None) => format!("{}cycles", c),
            (None, Some(d)) => format!("{}s", d),
            (None, None) => "default".to_string(),  // Match old behavior
        };

        // Mem-Sequence's runs of its passes: `2×(W 2R RW 2R@random)`, the default order apart
        let per_chunk = match crate::tests::effective_reps(test_def.actual_name, config) {
            Some((wrc, test_reps, verify_reps)) => format!("{wrc}×({test_reps}+{verify_reps})"),
            None if test_def.actual_name == "Mem-Sequence" => {
                let seq = config.sequence();
                let order = match seq.order {
                    crate::sequence::Order::Forward => String::new(),
                    other => format!(" @{}", other.name()),
                };
                format!("{}×({}){order}", config.write_read_cycles.max(1), seq.name())
            }
            None => "-".to_string(),
        };

        // Use the new formatter with calculated sizes for CacheLevel targets
        let mut extent_mode = formatter.format_extent_mode_with_size(&config.extent_mode, cache_info, thread_count);
        let mut chunk_mode = formatter.format_chunk_mode_with_size(&config.chunk_mode, cache_info, thread_count);

        // Resolved as the test resolves them (`TestRunner::new`): the extent floored to 4 KiB,
        // then the chunk from it, spread over it
        if least > 0 {
            let mut sized = config.clone();
            sized.thread_count = thread_count;
            let name = test_def.actual_name;
            // Below one granule TestRunner tests one granule (and logs it)
            let extent_at = |memory: usize| (sized.calculate_extent_size(name, memory) / GRANULE * GRANULE).max(GRANULE.min(memory));
            let (lo, hi) = (extent_at(least), extent_at(most));
            let sizes = if lo == hi { size(lo) } else { format!("{}-{}", size(lo), size(hi)) };
            extent_mode = match &config.extent_mode {
                ExtentMode::FullAllocation => format!("Full ({sizes}/thread)"),
                ExtentMode::Absolute { .. } => sizes,
                ExtentMode::CacheTotal { fraction } => format!("{sizes} (CacheTotal {fraction:.2}x)"),
                ExtentMode::Cache { target } => {
                    format!("{sizes} ({})", target.name_with_context(thread_count, cache_info.is_virtual_machine))
                }
            };
            let resolution = sized.resolve_chunk(name, lo);
            let spread = ChunkSpread::new(lo, resolution.resolved);
            let overlap = if spread.overlap() > 0 { format!(", {} overlap", size(spread.overlap())) } else { String::new() };
            let source = match &config.chunk_mode {
                ChunkMode::Tm5Block { divisor: 1, .. } => " (.cfg window)".to_string(),
                ChunkMode::Tm5Block { divisor, .. } => format!(" (.cfg window/{divisor})"),
                ChunkMode::Cache { target } => format!(" ({})", target.name()),
                ChunkMode::Auto => " (auto)".to_string(),
                ChunkMode::Whole => " (whole)".to_string(),
                ChunkMode::Absolute { .. } | ChunkMode::CacheTotal { .. } => String::new(),
            };
            chunk_mode = format!("{} x{}{overlap}{source}", size(spread.chunk()), spread.count());
            // Only when the memory is what limits the extent: a cache-sized extent smaller than
            // its chunk is the test's design (Mem-Refresh's 2 GiB chunk means "the whole extent")
            let memory_bounds_extent = sized.calculate_extent_size(name, 1 << 60) > lo;
            if resolution.shrunk_by_memory() && memory_bounds_extent {
                notes.push(format!("⚠  Chunk size was adjusted for test {} ({}) from {} -> {}, as the memory allocation ({} per thread) can't support it",
                                   i + 1, test_def.display_name, size(resolution.requested), size(resolution.resolved), size(lo)));
            }
        }

        use crate::runner::SealUse;
        let seal = match test_def.seal_use {
            _ if !config.seal.on => "",
            SealUse::Wrap if config.seal.wrap => "✓",
            SealUse::Data => "data",
            SealUse::Check => "check",
            _ => "–",
        }.to_string();

        let parameter = format_parameter_context(&config.parameter_context);
        let mut name = test_def.step_label(sequence.tm5);
        if steps > 1 {
            name.push_str(&format!(" ×{steps}"));
        }

        TestConfigurationEntry {
            number: i + 1,
            name,
            timing,
            per_chunk,
            parameter,
            extent_mode,
            chunk_mode,
            seal,
        }
    }).collect();

    // The sequence, when it isn't each test once in row order
    let label = |id: &str| if sequence.tm5 { id.to_string() } else { format!("\"{id}\"") };
    match sequence.order {
        Some(order) => {
            let steps: Vec<String> =
                order.iter().map(|(id, runs)| if *runs { label(id) } else { format!("({} off)", label(id)) }).collect();
            let repeats: Vec<String> = firsts
                .iter()
                .filter(|(_, n)| *n > 1)
                .map(|(d, n)| {
                    format!(
                        "{} ×{n}",
                        d.id.as_deref().map_or(d.display_name.clone(), |id| if sequence.tm5 { format!("Test {id}") } else { label(id) })
                    )
                })
                .collect();
            let extra = if repeats.is_empty() { String::new() } else { format!("; {}", repeats.join(", ")) };
            notes.push(format!("ℹ  Cycle: {} ({} steps{extra})", steps.join(" → "), test_definitions.len()));
        }
        None if unique != test_definitions.len() => {
            notes.push(format!("ℹ  Cycle: {} steps", test_definitions.len()));
        }
        None => {}
    }
    notes.push(match sequence.seal {
        Some((kernel, source)) => format!(
            "🔒 Seal ({}, {}): all memory is sealed at each cycle's start and checked at its end. ✓: the seal is \
                                 checked before every chunk and resealed after. data: the test works on the seal. –: never sealed; what it \
                                 overwrites is checked first and resealed before the next sealed step.",
            kernel.name(),
            source.name()
        ),
        None => format!("🔓 Seal: off ({})", sequence.seal_off_reason),
    });
    if let Some((width, source)) = sequence.width {
        notes.push(width_note(width, source, test_definitions));
    }

    TestConfigurationReport {
        suite_timing: suite_timing_str,
        test_count: unique,
        step_count: test_definitions.len(),
        sealed: sequence.seal.is_some(),
        tests,
        notes,
    }
}

/// Convert test summaries to cycle report
#[expect(dead_code, reason = "TODO #29: the per-cycle report is to be revived, not deleted (from TODO #69 F)")]
pub fn create_cycle_report(cycle: u32, duration_secs: u32, summaries: &[TestSummary]) -> CycleReport {
    let test_performances = summaries
        .iter()
        .enumerate()
        .map(|(i, summary)| TestPerformanceEntry {
            number: i + 1,
            name: summary.name.clone(),
            duration_secs: summary.duration_ms as f64 / 1000.0,
            data_processed_gib: bytes_to_gib_f64(summary.bytes_processed),
            throughput_mib_s: summary.throughput_mib_s,
            throughput_gib_s: summary.throughput_mib_s / 1024.0,
            errors: summary.errors,
            whea_total: summary.whea_total,
            whea_corrected: summary.whea_corrected,
        })
        .collect();

    CycleReport { cycle_number: cycle, duration_secs, test_performances }
}

/// The allocation report (TODO 81) from the stitched allocator's spans and the requests that
/// built them. Takes the spans so their request record moves in, and goes with the report.
pub fn create_allocation_report(
    spans: Vec<crate::memory::stitched::ThreadSpan>, elapsed: std::time::Duration, numa_nodes: u32,
) -> AllocationReport {
    use std::cmp::Reverse;
    use std::collections::BTreeMap;

    use crate::memory::allocator::PageSizeLevel;

    let mut threads: Vec<AllocationThread> = spans
        .into_iter()
        .map(|span| AllocationThread {
            thread_id: span.thread_id,
            numa_node: span.numa_node,
            target_bytes: span.target as u64,
            allocated_bytes: span.len as u64,
            huge_bytes: span.bytes_at(PageSizeLevel::Huge) as u64,
            large_bytes: span.bytes_at(PageSizeLevel::Large) as u64,
            regular_bytes: span.bytes_at(PageSizeLevel::Regular) as u64,
            remote_bytes: span.remote_bytes() as u64,
            commits: span.commits,
        })
        .collect();
    threads.sort_by_key(|t| t.thread_id);

    // Threads with no node sort last.
    let node_key = |node: Option<u32>| (node.is_none(), node);
    let mut nodes: BTreeMap<(bool, Option<u32>), AllocationNode> = BTreeMap::new();
    for t in &threads {
        let node = nodes.entry(node_key(t.numa_node)).or_insert_with(|| AllocationNode {
            numa_node: t.numa_node,
            threads: 0,
            target_bytes: 0,
            allocated_bytes: 0,
            huge_bytes: 0,
            large_bytes: 0,
            regular_bytes: 0,
            remote_bytes: 0,
            requests: 0,
            refused: 0,
        });
        node.threads += 1;
        node.target_bytes += t.target_bytes;
        node.allocated_bytes += t.allocated_bytes;
        node.huge_bytes += t.huge_bytes;
        node.large_bytes += t.large_bytes;
        node.regular_bytes += t.regular_bytes;
        node.remote_bytes += t.remote_bytes;
        node.requests += t.commits.len();
        node.refused += t.commits.iter().filter(|c| c.refused).count();
    }

    let mut groups: BTreeMap<_, (CommitGroup, HashSet<usize>)> = BTreeMap::new();
    for t in &threads {
        for c in &t.commits {
            let key = (Reverse(c.page), node_key(c.numa_node), node_key(t.numa_node), Reverse(c.len));
            let (group, users) = groups.entry(key).or_insert_with(|| {
                (
                    CommitGroup {
                        bytes: c.len as u64,
                        page: c.page,
                        numa_node: c.numa_node,
                        home_node: t.numa_node,
                        granted: 0,
                        refused: 0,
                        threads: 0,
                        granted_bytes: 0,
                    },
                    HashSet::new(),
                )
            });
            if c.refused {
                group.refused += 1;
            } else {
                group.granted += 1;
                group.granted_bytes += c.len as u64;
            }
            users.insert(t.thread_id);
        }
    }
    let commit_groups = groups
        .into_values()
        .map(|(mut group, users)| {
            group.threads = users.len();
            group
        })
        .collect();

    AllocationReport {
        allocator: "stitched".to_string(),
        elapsed,
        numa_nodes,
        fairness: allocation_fairness(&threads),
        nodes: nodes.into_values().collect(),
        commit_groups,
        threads,
    }
}

/// The middle value; the mean of the two middle ones when even. `None` when empty.
fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(sorted[n / 2]),
        _ => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0),
    }
}

/// Each dimension's values, every thread against the median of the others (TODO 81's rules).
fn allocation_fairness(threads: &[AllocationThread]) -> Vec<DimensionFairness> {
    let page = HUGE_PAGE_SIZE_USIZE as f64;
    FairnessDimension::ALL
        .iter()
        .map(|&dimension| {
            let values: Vec<f64> = threads
                .iter()
                .map(|t| match dimension {
                    FairnessDimension::Allocated => t.allocated_bytes as f64,
                    FairnessDimension::Huge => t.huge_bytes as f64,
                    FairnessDimension::Large => t.large_bytes as f64,
                    FairnessDimension::Regular => t.regular_bytes as f64,
                    FairnessDimension::Remote => t.remote_bytes as f64,
                    FairnessDimension::Requests => t.commits.len() as f64,
                })
                .collect();
            let cells = threads
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let others: Vec<f64> = values.iter().enumerate().filter(|&(j, _)| j != i).map(|(_, &v)| v).collect();
                    let value = values[i];
                    let median_others = median(&others);
                    let deviation = median_others.map_or(0.0, |m| value - m);
                    let (flag, badness) = match dimension {
                        FairnessDimension::Allocated => {
                            let short = t.target_bytes.saturating_sub(t.allocated_bytes) as f64;
                            let behind = median_others.is_some_and(|m| deviation < -0.01 * m);
                            let flag = if short > 0.0 || behind { FairnessFlag::Warn } else { FairnessFlag::Fine };
                            (flag, short.max(-deviation).max(0.0))
                        }
                        // More than one 1 GiB page fewer than the others: an even split promises
                        // one apart. Only the side that lost out, or every thread is flagged.
                        FairnessDimension::Huge => {
                            let flag = if deviation < -page { FairnessFlag::Notice } else { FairnessFlag::Fine };
                            (flag, (-deviation).max(0.0))
                        }
                        // Shown, never flagged: more 2 MiB pages is the 1 GiB shortfall again, and
                        // fewer is more 1 GiB pages or 4 KiB ones, which 4 KiB flags.
                        FairnessDimension::Large => (FairnessFlag::Fine, deviation.abs()),
                        FairnessDimension::Regular => {
                            let flag = if value > 0.0 { FairnessFlag::Notice } else { FairnessFlag::Fine };
                            (flag, value)
                        }
                        FairnessDimension::Remote => {
                            let share = if t.allocated_bytes > 0 { value / t.allocated_bytes as f64 } else { 0.0 };
                            let flag = match share {
                                s if s > 0.10 => FairnessFlag::Warn,
                                s if s > 0.0 => FairnessFlag::Notice,
                                _ => FairnessFlag::Fine,
                            };
                            (flag, share)
                        }
                        // Fed in small pieces: more than twice the others' requests.
                        FairnessDimension::Requests => {
                            let flag =
                                if median_others.is_some_and(|m| value > 2.0 * m) { FairnessFlag::Notice } else { FairnessFlag::Fine };
                            (flag, deviation.max(0.0))
                        }
                    };
                    FairnessCell { deviation, flag, badness }
                })
                .collect();
            DimensionFairness {
                dimension,
                cells,
                min: values.iter().copied().reduce(f64::min).unwrap_or(0.0),
                median: median(&values).unwrap_or(0.0),
                max: values.iter().copied().reduce(f64::max).unwrap_or(0.0),
            }
        })
        .collect()
}

/// Convert thread stats to thread timing report
///
/// `whea` is the count attributed to *this test* (the delta across it), not the run total — it is
/// what lets the aggregate row fail when the per-thread rows are all clean.
pub fn create_thread_timing_report(
    stats: &[(usize, usize, u64, std::time::Duration, u64, u64, u32)], // (thread_id, cpu_id, bytes, time, errors, operations, cycles_completed)
    whea: crate::whea::WheaCounts,
) -> super::models::ThreadTimingReport {
    use super::models::{ThreadTiming, ThreadTimingReport};

    if stats.is_empty() {
        return ThreadTimingReport { average_elapsed_ms: 0, thread_timings: Vec::new(), whea };
    }

    // Calculate averages
    let total_elapsed: std::time::Duration = stats.iter().map(|(_, _, _, elapsed, _, _, _)| *elapsed).sum();
    let avg_elapsed = (total_elapsed / stats.len() as u32).as_millis();

    let total_bytes: u64 = stats.iter().map(|(_, _, bytes, _, _, _, _)| *bytes).sum();
    let avg_bytes = total_bytes / stats.len() as u64;

    // Build thread timing entries with deviations
    let mut thread_timings: Vec<ThreadTiming> = stats
        .iter()
        .map(|&(thread_id, cpu_id, bytes, elapsed, errors, _operations, cycles_completed)| {
            let deviation_ms = elapsed.as_millis() as i128 - avg_elapsed as i128;
            let deviation_data_bytes = bytes as i64 - avg_bytes as i64;

            // Calculate throughput
            let throughput_mib_s = if !elapsed.is_zero() { (bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64() } else { 0.0 };

            // Get physical core and NUMA node info from the real detected topology
            // (NOT cpu_id/2 — that hardcoded 2-way SMT and mislabeled cores on non-SMT CPUs).
            let physical_core_id = crate::cpu_topology::get_physical_core_for_cpu(cpu_id);
            let numa_node = crate::cpu_topology::get_numa_node_for_cpu(cpu_id);

            ThreadTiming {
                thread_id,
                cpu_id,
                physical_core_id,
                numa_node,
                runtime_ms: elapsed.as_millis(),
                deviation_ms,
                data_bytes: bytes,
                deviation_data_bytes,
                throughput_mib_s,
                deviation_speed_mib_s: 0.0, // Will calculate after we know avg speed
                errors,
                cycles_completed,
            }
        })
        .collect();

    // Calculate average speed and then update speed deviations
    let avg_speed_mib_s = thread_timings.iter().map(|t| t.throughput_mib_s).sum::<f64>() / thread_timings.len() as f64;

    for timing in &mut thread_timings {
        timing.deviation_speed_mib_s = timing.throughput_mib_s - avg_speed_mib_s;
    }

    // Sort by deviation descending (highest deviation first)
    thread_timings.sort_by_key(|b| std::cmp::Reverse(b.deviation_ms));

    ThreadTimingReport { average_elapsed_ms: avg_elapsed, thread_timings, whea }
}

/// The final summary (TODO 74): one row per test, its steps combined (a test that runs twice a
/// cycle is one row with twice the runs), the seal's stages, and the steps that found errors.
pub fn create_overall_stats_summary_report(result: &crate::results::TestRunResult) -> super::models::FinalTestSummaryReport {
    use super::models::{FinalTestSummaryReport, SealStagesSummary, StepErrorsEntry, TestSummaryEntry};
    let overall_stats = &result.overall_stats;

    // Format runtime in fixed HH:MM:SS format for machine comparison
    let secs = overall_stats.total_runtime_secs;
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    let total_runtime = format!("{:02}:{:02}:{:02}", hours, minutes, seconds);

    // Steps of one test, combined: times and data per run, errors summed. A step's figures are
    // already per run (`TestAverage`), so the test's are its steps' run-weighted means.
    let mut per_test_summaries: Vec<TestSummaryEntry> = Vec::new();
    let mut seen: Vec<usize> = Vec::new();
    for avg in &overall_stats.per_step_averages {
        let label = match &avg.id {
            Some(_) => avg.label.clone(),
            None => avg.name.clone(),
        };
        let at = match seen.iter().position(|&t| t == avg.test_index) {
            Some(at) => at,
            None => {
                seen.push(avg.test_index);
                per_test_summaries.push(TestSummaryEntry {
                    name: label,
                    runs: 0,
                    seal_errors: 0,
                    average_duration_secs: 0.0,
                    average_data_gib: 0.0,
                    average_throughput_mib_s: 0.0,
                    total_errors: 0,
                    failing_bits: Default::default(),
                    whea_total: 0,
                    whea_corrected: 0,
                    // Copy latency data from the test's first step
                    latency_samples: avg.latency_samples,
                    latency_p5_ns: avg.latency_p5_ns,
                    latency_p10_ns: avg.latency_p10_ns,
                    latency_p25_ns: avg.latency_p25_ns,
                    latency_p50_ns: avg.latency_p50_ns,
                    latency_p75_ns: avg.latency_p75_ns,
                    latency_p90_ns: avg.latency_p90_ns,
                    latency_p95_ns: avg.latency_p95_ns,
                    latency_p99_ns: avg.latency_p99_ns,
                    latency_p99_9_ns: avg.latency_p99_9_ns,
                    latency_spread: avg.latency_spread,
                });
                per_test_summaries.len() - 1
            }
        };
        let entry = &mut per_test_summaries[at];
        let (old, new) = (entry.runs as f64, avg.runs as f64);
        // A step a halt stopped before it ran adds its seal errors and no figures
        let mean = |a: f64, b: f64| if old + new > 0.0 { (a * old + b * new) / (old + new) } else { 0.0 };
        entry.average_duration_secs = mean(entry.average_duration_secs, avg.avg_duration_ms as f64 / 1000.0);
        entry.average_data_gib = mean(entry.average_data_gib, avg.avg_bytes_processed as f64 / (1024.0 * 1024.0 * 1024.0));
        entry.average_throughput_mib_s = mean(entry.average_throughput_mib_s, avg.avg_throughput_mib_s);
        entry.runs += avg.runs;
        entry.total_errors += avg.total_errors;
        entry.failing_bits.add(&avg.failing_bits);
        entry.seal_errors += avg.total_seal_errors;
        entry.whea_total += avg.whea_total;
        entry.whea_corrected += avg.whea_corrected;
    }

    let sealed: Vec<&crate::results::CycleSeal> = result.cycles.iter().filter_map(|c| c.seal.as_ref()).collect();
    let seal = (!sealed.is_empty()).then(|| SealStagesSummary { final_check_errors: sealed.iter().map(|s| s.final_check_errors).sum() });
    // Every operation, the ones the run did none of too (TODO 90)
    let intermediate = crate::ops::Op::ALL
        .iter()
        .map(|&op| {
            let record = overall_stats.intermediate.get(op.key());
            super::models::IntermediateEntry {
                name: op.name(),
                secs: record.map(|r| r.ms / 1000.0),
                bytes: record.filter(|_| op.has_bytes()).map(|r| r.bytes),
            }
        })
        .collect();
    let errors_by_step = overall_stats
        .per_step_averages
        .iter()
        .filter(|s| s.total_errors + s.total_seal_errors > 0)
        .map(|s| StepErrorsEntry { step: s.step, label: s.label.clone(), errors: s.total_errors, seal_errors: s.total_seal_errors })
        .collect();

    FinalTestSummaryReport {
        total_runtime,
        cycles_completed: overall_stats.cycles_completed as usize,
        total_data_processed_gib: overall_stats.total_data_processed_gib,
        overall_throughput_mib_s: overall_stats.overall_throughput_mib_s,
        overall_throughput_gib_s: overall_stats.overall_throughput_mib_s / 1024.0,
        total_errors: overall_stats.total_errors,
        // Exact run-level counters (see TestRunResult::set_whea_totals) — not a sum of the
        // per-test figures, which miss events logged outside a test's execution window.
        whea_total: overall_stats.whea_total,
        whea_corrected: overall_stats.whea_corrected,
        whea_monitored: overall_stats.whea_monitored,
        per_test_summaries,
        seal,
        errors_by_step,
        failing_bits: overall_stats.failing_bits.clone(),
        seal_failing_bits: overall_stats.seal_failing_bits.clone(),
        intermediate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::allocator::PageSizeLevel::{Huge, Large};
    use crate::reporting::formatters::{DefaultFormatter, ReportFormatter};
    use crate::test_support::{allocation_report, allocation_report_one_node, allocation_report_two_nodes, repeat, span};

    const MIB: usize = 1 << 20;
    const GIB: usize = 1 << 30;

    /// TODO 99: a width given is in the plan, with where it came from, and Mem-Sequence's own
    /// where it lacks that one; under auto the plan says nothing of it.
    #[test]
    fn a_width_given_is_in_the_plan() {
        use crate::simd::Width;
        let cache_info = crate::tests::get_cache_info();
        let suite = crate::runner::create_test_definitions(cache_info);
        let pick = |names: &[&str]| suite.iter().filter(|d| names.contains(&d.actual_name)).cloned().collect::<Vec<_>>();
        let (with_sequence, without) = (pick(&["Mem-Sequence", "Mem-Random"]), pick(&["Mem-Random"]));
        assert_eq!((with_sequence.len(), without.len()), (2, 1));
        let notes = |defs: &[crate::runner::TestDefinition], width| {
            let sequence = PlanSequence { order: None, seal: None, seal_off_reason: "seal=off", tm5: false, width };
            create_test_configuration_report_v2(defs, &Default::default(), cache_info, &[64 * MIB], None, &sequence).notes
        };
        let width_notes = |notes: Vec<String>| notes.into_iter().filter(|n| n.contains("Width:")).collect::<Vec<_>>();

        assert!(width_notes(notes(&with_sequence, None)).is_empty(), "auto is the default: no note");
        let given = width_notes(notes(&with_sequence, Some((Width::W256, "width="))));
        assert!(given.len() == 1 && given[0].contains("256 bits (width=)") && !given[0].contains("has no"), "{given:?}");
        let given = width_notes(notes(&with_sequence, Some((Width::W128, "the config's width"))));
        assert!(
            given.len() == 1
                && given[0].contains("128 bits (the config's width)")
                && given[0].contains("Mem-Sequence has no 128-bit kernel and runs at 256"),
            "{given:?}"
        );
        let given = width_notes(notes(&without, Some((Width::W128, "width="))));
        assert!(given.len() == 1 && !given[0].contains("has no"), "no Mem-Sequence in the plan: {given:?}");
    }

    /// TODO 99: a sealed plan names the seal's kernel and where its width came from: calibrated,
    /// the rule, or the setting.
    #[test]
    fn the_plan_says_where_the_seals_width_came_from() {
        use crate::seal::{SealKernel, SealPattern, SealWidth, SealWidthSource};
        let cache_info = crate::tests::get_cache_info();
        let suite = crate::runner::create_test_definitions(cache_info);
        let defs: Vec<_> = suite.iter().filter(|d| d.actual_name == "Mem-Random").cloned().collect();
        let seal_note = |seal| {
            let sequence = PlanSequence { order: None, seal, seal_off_reason: "seal=off", tm5: false, width: None };
            let notes = create_test_configuration_report_v2(&defs, &Default::default(), cache_info, &[64 * MIB], None, &sequence).notes;
            notes.into_iter().find(|n| n.contains("Seal")).unwrap()
        };
        let kernel = |width| SealKernel::new(SealPattern::Tmr, width);
        assert!(seal_note(Some((kernel(SealWidth::W128), SealWidthSource::Calibrated))).starts_with("🔒 Seal (tmr-128, calibrated):"));
        assert!(seal_note(Some((kernel(SealWidth::W256), SealWidthSource::Rule))).starts_with("🔒 Seal (tmr-256, rule):"));
        assert!(
            seal_note(Some((kernel(SealWidth::W512), SealWidthSource::Setting("seal-width="))))
                .starts_with("🔒 Seal (tmr-512, seal-width=):")
        );
        assert_eq!(seal_note(None), "🔓 Seal: off (seal=off)");
    }

    fn cell<'a>(table: &'a TableData, row: usize, header: &str) -> &'a str {
        let col = table.headers.iter().position(|h| h.text == header).unwrap_or_else(|| panic!("no {header} column"));
        &table.rows[row][col]
    }

    /// The 2026-10-16 run on this box (`allocation_report_one_node`): its console golden is the
    /// CLI's (`report_allocation_one_node`, crates/tmr-cli).
    #[test]
    fn one_node_reads_as_local_and_fair() {
        let report = allocation_report_one_node();
        let f = DefaultFormatter::new();

        let nodes = f.prepare_allocation_nodes_table(&report);
        assert!(nodes.title.as_deref().unwrap().ends_with("1 NUMA node, all memory local"));
        assert!(nodes.headers.iter().all(|h| h.text != "Remote" && h.text != "Local"));
        assert_eq!(nodes.rows.len(), 1, "no Total row on one node");
        assert_eq!(cell(&nodes, 0, "1 GiB"), "18.00 GiB");
        assert_eq!(cell(&nodes, 0, "2 MiB"), "10.00 GiB");
        assert_eq!(cell(&nodes, 0, "Requests"), "99");
        assert_eq!(cell(&nodes, 0, "Refused"), "1");

        let commits = f.prepare_allocation_commits_table(&report);
        let rows: Vec<Vec<&str>> = commits.rows.iter().map(|r| r.iter().map(String::as_str).collect()).collect();
        assert_eq!(
            rows,
            vec![vec!["1024 MB", "1 GiB", "18", "1", "4", "18.00 GiB"], vec!["128 MB", "2 MiB", "80", "-", "4", "10.00 GiB"],]
        );

        let by_thread = f.prepare_thread_commits_table(&report);
        assert_eq!(cell(&by_thread, 0, "1 GiB"), "1024×5");
        assert_eq!(cell(&by_thread, 0, "2 MiB"), "128×16");
        assert_eq!(cell(&by_thread, 0, "4 KiB"), "-");
        assert_eq!(cell(&by_thread, 2, "Granted"), "28");
        assert_eq!(cell(&by_thread, 2, "Refused"), "1 (1 GiB: 1024)");

        // 5/5/4/4 pages is one page apart: fair.
        let threads = f.prepare_allocation_threads_table(&report);
        assert!(threads.rows.iter().all(|r| r.last().unwrap() == "✅"));
        let summary = f.prepare_fairness_summary_table(&report);
        assert!(summary.rows.iter().all(|r| r.last().unwrap() == "✅"));
        assert!(summary.rows.iter().all(|r| r[0] != "Remote"));
    }

    /// Two nodes, node 1 short of 1 GiB pages (`allocation_report_two_nodes`): its console golden is
    /// the CLI's (`report_allocation_two_nodes`, crates/tmr-cli).
    #[test]
    fn a_short_node_shows_in_every_table() {
        let report = allocation_report_two_nodes();
        let f = DefaultFormatter::new();

        let nodes = f.prepare_allocation_nodes_table(&report);
        assert_eq!(nodes.rows.len(), 3, "a Total row");
        assert_eq!(cell(&nodes, 1, "Remote"), "512.00 MiB (3.1%)");
        assert_eq!(cell(&nodes, 1, "Local"), "15.50 GiB");
        assert_eq!(cell(&nodes, 2, "Node"), "Total");
        assert_eq!(cell(&nodes, 2, "Refused"), "1");

        let commits = f.prepare_allocation_commits_table(&report);
        let remote = commits.rows.iter().find(|r| r[2] == "0 for 1").expect("a remote row");
        assert_eq!(remote, &["128 MB", "2 MiB", "0 for 1", "4", "-", "1", "512.00 MiB"]);

        let by_thread = f.prepare_thread_commits_table(&report);
        assert_eq!(cell(&by_thread, 3, "2 MiB"), "128×44+128@0×4");
        assert_eq!(cell(&by_thread, 3, "Refused"), "1 (2 MiB: 128)");

        // Node 0's threads got more 1 GiB pages, which is not their problem.
        let threads = f.prepare_allocation_threads_table(&report);
        assert_eq!(cell(&threads, 0, "Status"), "✅");
        assert_eq!(cell(&threads, 2, "Status"), "🟡 1 GiB -5.00 GiB, 50 requests");
        assert_eq!(cell(&threads, 3, "Status"), "🟡 1 GiB -5.00 GiB, remote 512.00 MiB (6.2%), 51 requests");

        let fairness = f.prepare_fairness_by_thread_table(&report);
        assert_eq!(cell(&fairness, 3, "Remote"), "+512.00 MiB 🟡");
        assert_eq!(cell(&fairness, 2, "Remote"), "=");
        let summary = f.prepare_fairness_summary_table(&report);
        let remote = summary.rows.iter().find(|r| r[0] == "Remote").unwrap();
        assert_eq!(remote[4], "3 (+512.00 MiB)");
        assert_eq!(remote[5], "🟡");
        let huge = summary.rows.iter().find(|r| r[0] == "1 GiB").unwrap();
        assert_eq!(huge[4], "2-3 (-5.00 GiB)");
        let large = summary.rows.iter().find(|r| r[0] == "2 MiB").unwrap();
        assert_eq!((large[4].as_str(), large[5].as_str()), ("0-3 (±5.00 GiB)", "✅"));
    }

    /// A thread short of its target is ⚠️, and one fed in small pieces is 🟡 on Requests.
    #[test]
    fn short_and_piecemeal_threads_are_flagged() {
        let mut small = repeat(3, GIB, Huge, 0);
        small.extend(repeat(64, 16 * MIB, Large, 0));
        let spans = vec![
            span(0, 0, 4 * GIB, &repeat(4, GIB, Huge, 0)),
            span(1, 0, 4 * GIB, &repeat(4, GIB, Huge, 0)),
            span(2, 0, 4 * GIB, &small),
            span(3, 0, 4 * GIB, &repeat(3, GIB, Huge, 0)),
        ];
        let report = allocation_report(spans, 1);
        let f = DefaultFormatter::new();
        let threads = f.prepare_allocation_threads_table(&report);
        // Its 1 GiB of 2 MiB pages is one page from the others': fair; 67 requests to their 4 aren't.
        assert_eq!(cell(&threads, 2, "Status"), "🟡 67 requests");
        assert_eq!(cell(&threads, 3, "Status"), "⚠️ short 1.00 GiB");
        let summary = f.prepare_fairness_summary_table(&report);
        let allocated = summary.rows.iter().find(|r| r[0] == "Allocated").unwrap();
        assert_eq!(allocated[4], "3 (-1.00 GiB)");
        assert_eq!(allocated[5], "⚠️");
    }
}
