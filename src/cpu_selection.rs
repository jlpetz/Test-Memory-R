//! Two-stage CPU selection: decide *which* logical CPUs the worker threads get pinned to.
//!
//! ```text
//! All cores ──[Stage 1: FILTER]──▶ Available pool ──[Stage 2: COUNT + SPACING]──▶ Assigned
//!               skip-cores=                            cpus= + cpu-stride=
//! ```
//!
//! **Stage 1 — FILTER** ([`resolve_skip_spec`]) decides which cores are *eligible*:
//! - `skip-cores=N`   — skip the first N cores (default 1, keeps the OS responsive)
//! - `skip-cores=N%`  — skip the leading N% (portable: valid on 4/6/8/16-core machines)
//! - `skip-cores=A-B` — exclude an inclusive core-id range
//!
//! **Stage 2 — SPACING** ([`apply_cpu_stride`]) picks the requested count out of what remains:
//! - `cpu-stride=1`    — densely packed (default / historical behaviour)
//! - `cpu-stride=N`    — every Nth eligible core
//! - `cpu-stride=even` — spread the count evenly across the whole pool
//!
//! `cputype=cores|threads` is also a Stage-1 prefilter (handled by the caller): stride operates
//! on the physical-core pool *after* SMT filtering, so `cpu-stride=2` means "every other core",
//! not "every other SMT sibling".
//!
//! # Why this matters (the motivating case)
//!
//! On multi-CCD parts, core groups can sit on **separate memory-bandwidth domains**. On a
//! 16-core EPYC 9R45 (AWS m8a.4xlarge) cores 0-7 and 8-15 each have their own ~40 GB/s pool, and
//! per-thread bandwidth ≈ `pool ÷ threads_in_that_pool`. Loading the pools unevenly (e.g. 4
//! threads in one, 2 in the other) produces wildly bimodal per-thread results that look like a
//! bug but are real hardware behaviour. `cpus=50% cpu-stride=even` selects
//! `[0,2,4,6,8,10,12,14]` — 4 threads per domain — giving uniform, comparable numbers.
//! `skip-cores=0-7` instead isolates a single domain for measurement.
//!
//! Selection **errors rather than silently right-sizing** when a count/stride combination can't
//! fit the pool: the `cpus=` percentage is the intended way to scale down.

use std::collections::HashMap;

use crate::cpu_topology::{CoreType, get_cpu_topology, is_hybrid_cpu};

/// Stage 1: resolve a `skip-cores` spec into a leading-skip count and/or an excluded id range.
///
/// Returns `(skip_count, excluded_range)`:
/// - `"N"`   → `(N, None)` — skip the first N cores
/// - `"N%"`  → `(round(N% of total), None)` — never skips *every* core
/// - `"A-B"` → `(0, Some((A, B)))` — exclude that inclusive core-id range
///
/// Unparseable input falls back to the historical default of skipping 1 core (the param layer
/// validates the shape up front, so this is belt-and-braces).
pub fn resolve_skip_spec(spec: &str, total_cores: usize) -> (usize, Option<(usize, usize)>) {
    let s = spec.trim();
    // Inclusive id range to exclude.
    if let Some((a, b)) = s.split_once('-')
        && let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
        return (0, Some((a, b)));
    }
    if let Some(pct) = s.strip_suffix('%')
        && let Ok(p) = pct.trim().parse::<u32>() {
        // Round to nearest, and never skip every core.
        let n = ((total_cores as u32 * p + 50) / 100) as usize;
        return (n.min(total_cores.saturating_sub(1)), None);
    }
    (s.parse::<usize>().unwrap_or(1), None)
}

/// Stage 2: pick `count` entries out of `pool` at the given spacing.
///
/// `stride_spec`: `"1"` = packed, `"N"` = every Nth, `"even"` = spread across the whole pool
/// (stride = `pool.len() / count`).
///
/// # Errors
/// Returns `Err` — never silently right-sizes — when the last pick would fall outside the pool
/// (`(count-1) * stride >= pool.len()`). Lower `cpus=` or the stride instead.
pub fn apply_cpu_stride(pool: &[usize], count: usize, stride_spec: &str) -> Result<Vec<usize>, String> {
    if count == 0 || pool.is_empty() {
        return Ok(Vec::new());
    }
    let stride = if stride_spec.trim().eq_ignore_ascii_case("even") {
        (pool.len() / count).max(1)
    } else {
        stride_spec.trim().parse::<usize>().unwrap_or(1).max(1)
    };

    // Required span of the pool: the last pick sits at index (count-1)*stride.
    let needed_span = (count - 1) * stride + 1;
    if needed_span > pool.len() {
        return Err(format!(
            "cpu-stride={} cannot place {} threads in {} available CPU(s): needs a span of {} \
             (last thread would land at index {}). Reduce cpus= or lower the stride.",
            stride_spec, count, pool.len(), needed_span, (count - 1) * stride
        ));
    }

    Ok((0..count).map(|i| pool[i * stride]).collect())
}

/// Full two-stage selection: returns `(thread_count, cpu_list)` where `cpu_list[i]` is the
/// logical CPU that worker `i` gets pinned to.
///
/// - `cpus_to_skip` / `excluded_range` — Stage 1 filter (see [`resolve_skip_spec`])
/// - `stride_spec` — Stage 2 spacing (see [`apply_cpu_stride`])
/// - `avoid_smt_doubling` — when true, at most one logical CPU per physical core
///
/// Hybrid (P/E-core) CPUs keep their existing behaviour: E-cores are skipped first, then
/// one-thread-per-P-core, then one-per-E-core, then SMT siblings. Spacing currently applies to
/// the non-hybrid path; on hybrid parts the core-type ordering already spreads the assignment.
pub fn calculate_thread_allocation(
    requested_threads: usize,
    cpus_to_skip: usize,
    avoid_smt_doubling: bool,
    excluded_range: Option<(usize, usize)>,
    stride_spec: &str,
) -> Result<(usize, Vec<usize>), String> {
    let topology = get_cpu_topology();
    let is_hybrid = is_hybrid_cpu(topology);

    // Group logical CPUs by physical core, honouring the Stage-1 excluded id range.
    let mut cores_map: HashMap<usize, Vec<(usize, CoreType)>> = HashMap::new();
    for cpu in topology {
        if let Some((lo, hi)) = excluded_range
            && cpu.physical_core_id >= lo && cpu.physical_core_id <= hi {
            continue; // filtered out before any spacing/count logic sees it
        }
        cores_map.entry(cpu.physical_core_id)
            .or_default()
            .push((cpu.logical_id, cpu.core_type));
    }

    // Separate P-cores and E-cores
    let mut p_cores: Vec<_> = cores_map.iter()
        .filter(|(_, cpus)| cpus.iter().any(|(_, t)| matches!(t, CoreType::Performance(_))))
        .map(|(id, cpus)| (*id, cpus.clone()))
        .collect();

    let mut e_cores: Vec<_> = cores_map.iter()
        .filter(|(_, cpus)| cpus.iter().any(|(_, t)| matches!(t, CoreType::Efficiency(_))))
        .map(|(id, cpus)| (*id, cpus.clone()))
        .collect();

    p_cores.sort_by_key(|(id, _)| *id);
    e_cores.sort_by_key(|(id, _)| *id);

    let mut selected_cpus = Vec::new();
    let mut cores_to_skip = cpus_to_skip;

    if is_hybrid {
        // Skip E-cores first (if we have any to skip)
        let e_cores_to_skip = cores_to_skip.min(e_cores.len());
        let e_cores_to_use = e_cores.iter().skip(e_cores_to_skip);

        // Update remaining cores to skip
        cores_to_skip = cores_to_skip.saturating_sub(e_cores.len());

        // Then skip some P-cores if needed
        let p_cores_to_skip = cores_to_skip.min(p_cores.len());
        let p_cores_to_use = p_cores.iter().skip(p_cores_to_skip);

        // Round-robin assignment: spread threads across physical cores first, then fill in SMT
        // siblings if more threads are needed (better utilisation at partial CPU counts).
        let p_cores_vec: Vec<_> = p_cores_to_use.collect();
        let e_cores_vec: Vec<_> = e_cores_to_use.collect();

        // Pass 1: Take first thread from each P-core
        for (_, logical_cpus) in &p_cores_vec {
            if selected_cpus.len() >= requested_threads { break; }
            if let Some((cpu_id, _)) = logical_cpus.first() {
                selected_cpus.push(*cpu_id);
            }
        }

        // Pass 2: Take first thread from each E-core
        for (_, logical_cpus) in &e_cores_vec {
            if selected_cpus.len() >= requested_threads { break; }
            if let Some((cpu_id, _)) = logical_cpus.first() {
                selected_cpus.push(*cpu_id);
            }
        }

        // Pass 3 & 4: If not avoiding SMT and still need more, take SMT siblings
        if !avoid_smt_doubling && selected_cpus.len() < requested_threads {
            for (_, logical_cpus) in &p_cores_vec {
                for (cpu_id, _) in logical_cpus.iter().skip(1) {
                    if selected_cpus.len() >= requested_threads { break; }
                    selected_cpus.push(*cpu_id);
                }
            }
            for (_, logical_cpus) in &e_cores_vec {
                for (cpu_id, _) in logical_cpus.iter().skip(1) {
                    if selected_cpus.len() >= requested_threads { break; }
                    selected_cpus.push(*cpu_id);
                }
            }
        }
    } else {
        // Non-hybrid CPU
        let mut physical_cores: Vec<_> = cores_map.keys().cloned().collect();
        physical_cores.sort();

        // Stage 1: skip the first N cores (range exclusion already applied to cores_map).
        let cores_to_use: Vec<_> = physical_cores.iter().skip(cores_to_skip).cloned().collect();

        // Stage 2: apply spacing across the available pool. The primary pass takes the first
        // logical CPU of each selected physical core.
        let first_cpu_pool: Vec<usize> = cores_to_use.iter()
            .filter_map(|pc| cores_map.get(pc).and_then(|l| l.first()).map(|(id, _)| *id))
            .collect();

        let primary_count = requested_threads.min(first_cpu_pool.len());
        let strided = apply_cpu_stride(&first_cpu_pool, primary_count, stride_spec)?;
        selected_cpus.extend(strided);

        // If not avoiding SMT and still short, take SMT siblings of the cores we actually
        // selected (keeps siblings with their primary, honouring the spacing).
        if !avoid_smt_doubling && selected_cpus.len() < requested_threads {
            let chosen: Vec<usize> = selected_cpus.clone();
            for primary in &chosen {
                if let Some((_, logical_cpus)) = cores_map.iter()
                    .find(|(_, l)| l.first().map(|(id, _)| id == primary).unwrap_or(false)) {
                    for (cpu_id, _) in logical_cpus.iter().skip(1) {
                        if selected_cpus.len() >= requested_threads { break; }
                        selected_cpus.push(*cpu_id);
                    }
                }
                if selected_cpus.len() >= requested_threads { break; }
            }
        }
    }

    Ok((selected_cpus.len(), selected_cpus))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Stage 1: skip specs ----

    #[test]
    fn skip_absolute() {
        assert_eq!(resolve_skip_spec("0", 16), (0, None));
        assert_eq!(resolve_skip_spec("4", 16), (4, None));
    }

    #[test]
    fn skip_percentage_is_portable_across_core_counts() {
        // Same spec, valid on every machine size — the reason `%` is relative.
        assert_eq!(resolve_skip_spec("25%", 4).0, 1);
        assert_eq!(resolve_skip_spec("25%", 6).0, 2); // rounds to nearest
        assert_eq!(resolve_skip_spec("25%", 8).0, 2);
        assert_eq!(resolve_skip_spec("25%", 16).0, 4);
    }

    #[test]
    fn skip_percentage_never_skips_every_core() {
        assert_eq!(resolve_skip_spec("100%", 8).0, 7);
    }

    #[test]
    fn skip_range_excludes_ids_not_a_count() {
        assert_eq!(resolve_skip_spec("0-7", 16), (0, Some((0, 7))));
        assert_eq!(resolve_skip_spec("8-15", 16), (0, Some((8, 15))));
    }

    // ---- Stage 2: spacing ----

    #[test]
    fn stride_one_is_packed() {
        let pool: Vec<usize> = (0..16).collect();
        assert_eq!(apply_cpu_stride(&pool, 4, "1").unwrap(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn stride_n_takes_every_nth() {
        let pool: Vec<usize> = (0..16).collect();
        assert_eq!(apply_cpu_stride(&pool, 8, "2").unwrap(), vec![0, 2, 4, 6, 8, 10, 12, 14]);
    }

    #[test]
    fn stride_even_spreads_across_both_memory_domains() {
        // The motivating case: 8 threads on a 16-core 2-domain part → 4 per domain.
        let pool: Vec<usize> = (0..16).collect();
        let picked = apply_cpu_stride(&pool, 8, "even").unwrap();
        assert_eq!(picked, vec![0, 2, 4, 6, 8, 10, 12, 14]);
        assert_eq!(picked.iter().filter(|&&c| c < 8).count(), 4, "lower domain");
        assert_eq!(picked.iter().filter(|&&c| c >= 8).count(), 4, "upper domain");
    }

    #[test]
    fn stride_errors_instead_of_right_sizing() {
        let pool: Vec<usize> = (0..16).collect();
        // 8 threads at stride 4 needs a span of 29 — must fail, not silently clamp.
        let err = apply_cpu_stride(&pool, 8, "4").unwrap_err();
        assert!(err.contains("cannot place 8 threads"), "unexpected: {err}");
    }

    #[test]
    fn stride_exact_fit_is_allowed() {
        let pool: Vec<usize> = (0..16).collect();
        // last index = (4-1)*5 = 15 → exactly the final slot
        assert_eq!(apply_cpu_stride(&pool, 4, "5").unwrap(), vec![0, 5, 10, 15]);
    }

    #[test]
    fn stride_degenerate_inputs() {
        let pool: Vec<usize> = (0..8).collect();
        assert!(apply_cpu_stride(&pool, 0, "2").unwrap().is_empty());
        assert!(apply_cpu_stride(&[], 4, "1").unwrap().is_empty());
    }
}
