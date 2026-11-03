//! CPU selection: which logical CPUs the worker threads get pinned to, from one ordered list of the cores (TODO 108).
//!
//! ```text
//! The cores ──[ORDER]──▶ ranked list ──[skip-cores=]──▶ pool ──[cpus= (+ cpu-stride=)]──▶ each thread's CPU
//! ```
//!
//! **The order** ([`rank`]): performance cores before efficiency cores when the cores have types; then, within each group of cores that
//! share an L3 (Windows' CPU-set LastLevelCacheIndex), Windows' core priority (the CPU set's SchedulingClass) highest first, ties by core
//! number; and the groups interleaved, the best of each in turn, then the second of each, and so on, alternating NUMA nodes. On a desktop
//! that is AMD's preferred-core order (an 8600G's priorities 3 2 1 4 0 4 put cores 3 and 5 first and core 4 last), on a server with
//! every priority 0 it is core-number order within each L3, and across L3s it balances the memory domains (an m8a.4xlarge's cores 0-7
//! and 8-15 are two ~40 GB/s pools in one NUMA node: half its cores is 4 in each, which `cpu-stride=even` was added for, TODO 30).
//!
//! **`skip-cores=`** ([`SkipSpec`]) leaves cores out, all the counts below and the start-up table taking the same list:
//! - `N`: the last N cores in the order, the weakest (default 1, for the system's responsiveness)
//! - `N%`: N% of the cores, rounded, from the end too; never every core
//! - `A-B`: the cores numbered A to B, wherever they rank (to isolate a memory domain)
//!
//! **`cpus=`** takes the threads from the front of what's left: a share of it (`50%`) or a count, at least 1 and at most all. Under
//! `cputype=threads` each core's first CPU comes before any core's second, so siblings stay paired and a half-size run puts one thread a
//! core. **`cpu-stride=`**, when given, keeps its meaning from TODO 30: it takes what's left by core number instead of the ranked order,
//! and spaces the picks by position (`N` every Nth, `even` the count spread over all, `1` packed), core types or not.
//!
//! Selection **errors rather than silently right-sizing** when a skip leaves no core, or a count and stride can't fit what's left.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::cpu_topology::{CoreType, CpuTopologyInfo, L3Group};
use crate::reporting::models::{CpuTopologyEntry, CpuTopologyReport, TopologySummary};

/// One physical core, as the selection ranks it: from the detected topology ([`cores`]), or made up in a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Core {
    /// TMR's core number (Windows' record order)
    pub id: usize,
    /// Its logical CPUs, ascending (TMR's numbering, group * 64 + bit); never empty
    pub cpus: Vec<usize>,
    pub core_type: CoreType,
    /// Windows' core priority, the CPU set's SchedulingClass, the highest of its CPUs': higher is preferred
    pub priority: u8,
    /// The cores sharing its last-level cache
    pub l3: L3Group,
    pub numa_node: u32,
}

/// The cores of the detected topology, by core number, each with what ranks it (its first CPU's type, L3 and node).
pub fn cores(topology: &[CpuTopologyInfo]) -> Vec<Core> {
    let mut by_id: BTreeMap<usize, Core> = BTreeMap::new();
    for cpu in topology {
        let core = by_id.entry(cpu.physical_core_id).or_insert_with(|| Core {
            id: cpu.physical_core_id,
            cpus: Vec::new(),
            core_type: cpu.core_type,
            priority: cpu.priority,
            l3: cpu.l3,
            numa_node: cpu.numa_node,
        });
        core.cpus.push(cpu.logical_id);
        core.priority = core.priority.max(cpu.priority);
    }
    let mut cores: Vec<Core> = by_id.into_values().collect();
    for core in &mut cores {
        core.cpus.sort_unstable();
    }
    cores
}

/// `skip-cores=`: which cores the run leaves out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipSpec {
    /// `N`: the last N cores in the order, the weakest
    Count(usize),
    /// `N%`: N% of the cores, rounded to the nearest, from the end of the order; never every core
    Percent(u32),
    /// `A-B`: the cores numbered A to B, inclusive
    Range(usize, usize),
}

impl SkipSpec {
    /// From a `skip-cores=` value: `N`, `N%` (at most 100) or `A-B` (A at most B). The parameter's parser, and the session's.
    pub fn parse(value: &str) -> Result<Self, String> {
        let s = value.trim();
        if let Some((a, b)) = s.split_once('-') {
            let a = a.trim().parse::<usize>().map_err(|_| format!("Invalid skip-cores range start '{}' in '{}'", a, s))?;
            let b = b.trim().parse::<usize>().map_err(|_| format!("Invalid skip-cores range end '{}' in '{}'", b, s))?;
            if a > b {
                return Err(format!("Invalid skip-cores range '{}': start {} > end {}", s, a, b));
            }
            Ok(SkipSpec::Range(a, b))
        } else if let Some(pct) = s.strip_suffix('%') {
            let p = pct.trim().parse::<u32>().map_err(|_| format!("Invalid skip-cores percentage '{}'", s))?;
            if p > 100 {
                return Err(format!("Invalid skip-cores '{}': percentage must be <= 100", s));
            }
            Ok(SkipSpec::Percent(p))
        } else {
            s.parse::<usize>().map(SkipSpec::Count).map_err(|_| format!("Invalid skip-cores value '{}' (expected N, N%, or A-B)", s))
        }
    }

    /// Whether it counts from the end of the order (`N`, `N%`), not a range of core numbers
    pub fn from_the_end(self) -> bool {
        !matches!(self, SkipSpec::Range(..))
    }
}

impl std::fmt::Display for SkipSpec {
    /// The `skip-cores=` value
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipSpec::Count(n) => write!(f, "{n}"),
            SkipSpec::Percent(p) => write!(f, "{p}%"),
            SkipSpec::Range(a, b) => write!(f, "{a}-{b}"),
        }
    }
}

/// Where a core's type puts it in the order: performance cores first (tier 0 first), then efficiency cores (the highest class first),
/// then cores of no known type.
fn type_rank(core_type: CoreType) -> (u8, u8) {
    match core_type {
        CoreType::Performance(tier) => (0, tier),
        CoreType::Efficiency(tier) => (1, u8::MAX - tier),
        CoreType::Unknown => (2, 0),
    }
}

/// The cores in the run's order, as indices into `cores` (TODO 108): by type first (performance, then efficiency, then unknown; one type
/// when the cores have none), and within a type the cores sharing an L3 ranked by priority, highest first, ties by core number, the
/// groups interleaved: the best of each group in turn, then the second of each, and so on. The groups take their turns by their lowest
/// core, alternating NUMA nodes (each node's first group, then each node's second...), so a count short of a whole round still splits
/// evenly over the nodes. `skip-cores=N` takes from the end, `cpus=` from the front.
pub fn rank(cores: &[Core]) -> Vec<usize> {
    let mut types: Vec<(u8, u8)> = cores.iter().map(|core| type_rank(core.core_type)).collect();
    types.sort_unstable();
    types.dedup();
    let mut order = Vec::with_capacity(cores.len());
    for kind in types {
        let mut groups: BTreeMap<L3Group, Vec<usize>> = BTreeMap::new();
        for (i, core) in cores.iter().enumerate().filter(|(_, core)| type_rank(core.core_type) == kind) {
            groups.entry(core.l3).or_default().push(i);
        }
        // Each group's lowest core and its node, which place the group's turn
        let mut groups: Vec<(usize, u32, Vec<usize>)> = groups
            .into_values()
            .map(|members| {
                let lowest = members.iter().copied().min_by_key(|&i| cores[i].id).expect("a group has a core");
                (cores[lowest].id, cores[lowest].numa_node, members)
            })
            .collect();
        groups.sort_unstable_by_key(|&(lowest, ..)| lowest);
        let mut nth_in_node: HashMap<u32, usize> = HashMap::new();
        let mut turns: Vec<((usize, u32, usize), Vec<usize>)> = groups
            .into_iter()
            .map(|(lowest, node, mut members)| {
                members.sort_by_key(|&i| (Reverse(cores[i].priority), cores[i].id));
                let nth = nth_in_node.entry(node).or_default();
                *nth += 1;
                ((*nth, node, lowest), members)
            })
            .collect();
        turns.sort_unstable_by_key(|&(turn, _)| turn);
        let rounds = turns.iter().map(|(_, members)| members.len()).max().unwrap_or(0);
        for round in 0..rounds {
            order.extend(turns.iter().filter_map(|(_, members)| members.get(round)));
        }
    }
    order
}

/// What a run asks of the selection: its `skip-cores=`, `cpus=`, `cpu-stride=` and `cputype=`.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub skip: SkipSpec,
    /// `cpus=`: `N%` or `N`
    pub cpus: &'a str,
    /// `cpu-stride=`, when given: the cores by number, spaced by it
    pub stride: Option<&'a str>,
    /// `cputype=cores`: `cpus=` counts cores, else logical CPUs
    pub count_cores: bool,
    /// At most one CPU a core: `cputype=cores`, or a config's `avoid_smt_doubling`
    pub one_per_core: bool,
}

/// The selection: the order, what `skip-cores=` left out, and each thread's CPU. The count, the choice and the start-up table all come
/// from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// Every core, in the ranked order ([`rank`]), as core numbers
    pub order: Vec<usize>,
    /// The cores `skip-cores=` left out, ascending
    pub skipped: Vec<usize>,
    /// What `cpus=` is a share of: the logical CPUs left (`cputype=cores`: the cores)
    pub available: usize,
    /// The threads `cpus=` asks for, 1 to `available`
    pub threads: usize,
    /// `cpus=` as a share: its percent
    pub percent: Option<u32>,
    /// Each thread's logical CPU, in thread order
    pub cpu_list: Vec<usize>,
    /// The most threads any one core runs: a cache-sized extent is shared by them (`tests::set_active_threads_per_core`)
    pub threads_per_core: usize,
}

/// The threads `cpus=` asks for out of `available` (at least 1), and its percent when it is a share. A share rounds to the nearest and a
/// count is taken as it is, both clamped to 1..=`available`: `0` and `0%` are one thread, more than all is all. Computed in u128, so no
/// share overflows (`cpus=` is checked at parsing, 31d510c, and `u32::MAX%` passes it).
pub fn thread_count(cpus: &str, available: usize) -> (usize, Option<u32>) {
    let available = available.max(1);
    match cpus.trim().strip_suffix('%') {
        Some(share) => {
            let percent = share.trim().parse::<u32>().unwrap_or(100);
            let n = (available as u128 * u128::from(percent) + 50) / 100;
            (n.clamp(1, available as u128) as usize, Some(percent))
        }
        None => (cpus.trim().parse::<usize>().unwrap_or(available).clamp(1, available), None),
    }
}

/// Pick `count` entries out of `pool` at the given spacing (`cpu-stride=`).
///
/// `stride_spec`: `"1"` = packed, `"N"` = every Nth, `"even"` = spread across the whole pool (stride = `pool.len() / count`).
///
/// # Errors
/// Returns `Err` — never silently right-sizes — when the last pick would fall outside the pool (`(count-1) * stride >= pool.len()`). Lower
/// `cpus=` or the stride instead.
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
            stride_spec,
            count,
            pool.len(),
            needed_span,
            (count - 1) * stride
        ));
    }

    Ok((0..count).map(|i| pool[i * stride]).collect())
}

/// The selection for `request` on `cores` (TODO 108): the cores ranked ([`rank`]), `skip-cores=` leaving out the end of the order or a
/// range of core numbers, then the threads from the front of what's left, or by core number at `cpu-stride=`'s spacing when it is given.
/// Each chosen core's first CPU comes first, then, unless one a core, their siblings in the same order, until `cpus=`'s count.
///
/// # Errors
/// When the skip leaves no core, or the stride can't place the count ([`apply_cpu_stride`]): the session stops with it.
pub fn choose(cores: &[Core], request: &Request) -> Result<Choice, String> {
    if cores.is_empty() {
        return Err("No cores found to run on".to_string());
    }
    let order = rank(cores);
    let from_end = |n: usize| order[cores.len() - n..].iter().copied().collect::<HashSet<usize>>();
    let left_out: HashSet<usize> = match request.skip {
        SkipSpec::Count(n) if n >= cores.len() => {
            return Err(format!(
                "skip-cores={n} leaves no core to test: this CPU has {}. Lower it, or give a share (skip-cores=25%), which never \
                 leaves out every core",
                cores.len()
            ));
        }
        SkipSpec::Count(n) => from_end(n),
        SkipSpec::Percent(p) => from_end(((cores.len() as u64 * u64::from(p) + 50) / 100).min(cores.len() as u64 - 1) as usize),
        SkipSpec::Range(lo, hi) => (0..cores.len()).filter(|&i| (lo..=hi).contains(&cores[i].id)).collect(),
    };
    let mut pool: Vec<usize> = order.iter().copied().filter(|i| !left_out.contains(i)).collect();
    if pool.is_empty() {
        let all: Vec<usize> = cores.iter().map(|core| core.id).collect();
        return Err(format!("skip-cores={} leaves no core to test: this CPU's are {}", request.skip, core_list(&all)));
    }
    if request.stride.is_some() {
        pool.sort_by_key(|&i| cores[i].id);
    }

    let available = if request.count_cores { pool.len() } else { pool.iter().map(|&i| cores[i].cpus.len()).sum() };
    let (threads, percent) = thread_count(request.cpus, available);
    let primaries = threads.min(pool.len());
    let picked = match request.stride {
        Some(stride) => apply_cpu_stride(&pool, primaries, stride)?,
        None => pool[..primaries].to_vec(),
    };
    let mut cpu_list: Vec<usize> = picked.iter().filter_map(|&i| cores[i].cpus.first().copied()).collect();
    if !request.one_per_core {
        let siblings = picked.iter().flat_map(|&i| cores[i].cpus.iter().skip(1).copied());
        let room = threads.saturating_sub(cpu_list.len());
        cpu_list.extend(siblings.take(room));
    }
    let chosen: HashSet<usize> = cpu_list.iter().copied().collect();
    let threads_per_core = picked.iter().map(|&i| cores[i].cpus.iter().filter(|cpu| chosen.contains(cpu)).count()).max().unwrap_or(1);

    let mut skipped: Vec<usize> = left_out.iter().map(|&i| cores[i].id).collect();
    skipped.sort_unstable();
    Ok(Choice { order: order.iter().map(|&i| cores[i].id).collect(), skipped, available, threads, percent, cpu_list, threads_per_core })
}

/// "core 3", "cores 0 and 4", "cores 0, 2 and 4", "cores 0-7", "cores 0-3 and 8": ascending, three or more in a row as their ends.
pub fn core_list(cores: &[usize]) -> String {
    let mut sorted = cores.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let mut end = i;
        while end + 1 < sorted.len() && sorted[end + 1] == sorted[end] + 1 {
            end += 1;
        }
        if end - i >= 2 {
            parts.push(format!("{}-{}", sorted[i], sorted[end]));
            i = end + 1;
        } else {
            parts.push(sorted[i].to_string());
            i += 1;
        }
    }
    match parts.as_slice() {
        [] => "no core".to_string(),
        [one] if sorted.len() == 1 => format!("core {one}"),
        [one] => format!("cores {one}"),
        [rest @ .., last] => format!("cores {} and {last}", rest.join(", ")),
    }
}

/// The CPUs at startup (TODO 29: `Setup::CpuTopology`), from the selection (TODO 108): each CPU's core, type, priority, L3 group and
/// NUMA node, and what the run does with it (skips it, assigns it a thread, leaves it out as an assigned core's SMT sibling, or leaves it
/// free). On a hybrid CPU (`hybrid`, `cpu_topology::is_hybrid_cpu`), also how many of its P-cores have SMT (`None` on one that isn't),
/// and whether skipping took E-cores first: a skip from the end of the order, which puts them last.
pub fn startup_topology(
    cores: &[Core], request: &Request, choice: &Choice, pinned: bool, hybrid: bool,
) -> (CpuTopologyReport, Option<usize>, bool) {
    let thread_of: HashMap<usize, usize> = choice.cpu_list.iter().enumerate().map(|(thread, &cpu)| (cpu, thread)).collect();
    let skipped: HashSet<usize> = choice.skipped.iter().copied().collect();
    // The L3 groups numbered from the lowest core up (`cores` is by core number)
    let mut l3_numbers: HashMap<L3Group, usize> = HashMap::new();
    for core in cores {
        let next = l3_numbers.len();
        l3_numbers.entry(core.l3).or_insert(next);
    }

    let (mut assigned_count, mut available_count, mut skipped_count, mut smt_excluded_count) = (0, 0, 0, 0);
    let mut entries = Vec::new();
    for core in cores {
        let core_assigned = core.cpus.iter().any(|cpu| thread_of.contains_key(cpu));
        for &cpu in &core.cpus {
            let thread_id = thread_of.get(&cpu).copied();
            let status = if skipped.contains(&core.id) {
                skipped_count += 1;
                "Skipped"
            } else if thread_id.is_some() {
                assigned_count += 1;
                "Assigned"
            } else if request.one_per_core && core_assigned {
                // An SMT sibling of an assigned core, left out by cputype=cores
                smt_excluded_count += 1;
                "SMT Excluded"
            } else {
                available_count += 1;
                "Available"
            };
            entries.push(CpuTopologyEntry {
                logical_cpu: cpu,
                physical_core: core.id,
                core_type: core.core_type.display_name(),
                priority: core.priority,
                l3_group: l3_numbers[&core.l3],
                threads_on_core: core.cpus.len(),
                numa_node: core.numa_node,
                is_hyperthreaded: core.cpus.len() > 1,
                status: status.to_string(),
                thread_id,
            });
        }
    }
    entries.sort_by_key(|entry| entry.logical_cpu);

    let of_type = |want: fn(&CoreType) -> bool| cores.iter().filter(move |core| want(&core.core_type));
    let is_p = |t: &CoreType| matches!(t, CoreType::Performance(_));
    let is_e = |t: &CoreType| matches!(t, CoreType::Efficiency(_));
    let summary = TopologySummary {
        is_hybrid: hybrid,
        total_logical: entries.len(),
        total_physical: cores.len(),
        performance_cores: of_type(is_p).count(),
        efficiency_cores: of_type(is_e).count(),
        performance_logical: of_type(is_p).map(|core| core.cpus.len()).sum(),
        efficiency_logical: of_type(is_e).map(|core| core.cpus.len()).sum(),
        assigned_count,
        available_count,
        skipped_count,
        smt_excluded_count,
    };
    let report = CpuTopologyReport { cpus: entries, summary, pinned, stride: request.stride.map(str::to_string) };
    let p_cores_with_smt = hybrid.then(|| of_type(is_p).filter(|core| core.cpus.len() > 1).count());
    let e_cores_skipped_first = hybrid && request.skip.from_the_end() && !choice.skipped.is_empty();
    (report, p_cores_with_smt, e_cores_skipped_first)
}

/// Made-up cores, for the tests and the CLI's goldens: core `i` has priority `priorities[i]`, `per_l3` cores share an L3 and `per_node` a
/// NUMA node (both in core order), and two CPUs (`2i`, `2i + 1`) with `smt`, else one (`i`). Every core a performance core; a test types
/// them as it needs.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn synthetic(priorities: &[u8], per_l3: usize, per_node: usize, smt: bool) -> Vec<Core> {
    priorities
        .iter()
        .enumerate()
        .map(|(i, &priority)| {
            let cpus = if smt { vec![2 * i, 2 * i + 1] } else { vec![i] };
            // The index is group-relative in Windows, so the key carries the processor group
            Core {
                id: i,
                l3: ((cpus[0] / 64) as u16, (i / per_l3) as u8),
                cpus,
                core_type: CoreType::Performance(0),
                priority,
                numa_node: (i / per_node) as u32,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_topology::CoreType::{Efficiency as E, Performance as P};

    const R8600G: [u8; 6] = [3, 2, 1, 4, 0, 4];
    const R5700X: [u8; 8] = [1, 6, 3, 5, 6, 2, 0, 4];
    const R8500G: [u8; 6] = [1, 0, 0, 0, 1, 0];

    /// A request: `skip-cores=skip`, `cpus=`, `cpu-stride=` when given, `cputype=cores` when `cores`
    fn request<'a>(skip: &str, cpus: &'a str, stride: Option<&'a str>, cores: bool) -> Request<'a> {
        Request { skip: SkipSpec::parse(skip).unwrap(), cpus, stride, count_cores: cores, one_per_core: cores }
    }

    fn pick(cores: &[Core], skip: &str, cpus: &str, stride: Option<&str>, cputype_cores: bool) -> Choice {
        choose(cores, &request(skip, cpus, stride, cputype_cores)).unwrap()
    }

    /// The cores the CPUs in `cpus` are on, in thread order
    fn cores_of(cores: &[Core], cpus: &[usize]) -> Vec<usize> {
        cpus.iter().map(|cpu| cores.iter().find(|core| core.cpus.contains(cpu)).unwrap().id).collect()
    }

    /// An Intel hybrid: 8 P-cores with SMT (cores 0-7, CPUs 0-15), 16 E-cores without (cores 8-23, CPUs 16-31), one L3; Windows favours
    /// P-cores 2 and 5
    fn intel_hybrid() -> Vec<Core> {
        (0..24)
            .map(|i| Core {
                id: i,
                cpus: if i < 8 { vec![2 * i, 2 * i + 1] } else { vec![i + 8] },
                core_type: if i < 8 { P(0) } else { E(0) },
                priority: u8::from(i == 2 || i == 5),
                l3: (0, 0),
                numa_node: 0,
            })
            .collect()
    }

    // ---- The order ----

    /// The desktops' priorities (TODO 108's table), one L3 each: highest first, ties by core number. The 8500G's Zen 4 cores (0 and 4)
    /// come first, its Zen 4c cores after.
    #[test]
    fn desktops_rank_by_windows_priority() {
        let order = |priorities: &[u8], smt| pick(&synthetic(priorities, 64, 64, smt), "0", "100%", None, true).order;
        assert_eq!(order(&R8600G, true), [3, 5, 0, 1, 2, 4]);
        assert_eq!(order(&R5700X, true), [1, 4, 3, 7, 2, 5, 0, 6]);
        assert_eq!(order(&R8500G, true), [0, 4, 1, 2, 3, 5]);
        assert_eq!(order(&R8600G, false), [3, 5, 0, 1, 2, 4], "SMT off: the same order");
        assert_eq!(order(&[0; 8], true), [0, 1, 2, 3, 4, 5, 6, 7], "a server: every priority 0, core-number order");
    }

    /// The m8a.4xlarge: 16 cores in 2 L3 groups of 8 in one NUMA node, every priority 0. The groups take turns, so half the cores is 4
    /// in each memory domain, with the default skip-cores=1 too (cpu-stride=even packed 7 + 1 there, TODO 108).
    #[test]
    fn two_l3_groups_in_one_node_take_turns() {
        let cores = synthetic(&[0; 16], 8, 16, false);
        let choice = pick(&cores, "1", "50%", None, true);
        assert_eq!(choice.order, [0, 8, 1, 9, 2, 10, 3, 11, 4, 12, 5, 13, 6, 14, 7, 15]);
        assert_eq!(choice.skipped, [15]);
        assert_eq!((choice.available, choice.threads), (15, 8));
        assert_eq!(choice.cpu_list, [0, 8, 1, 9, 2, 10, 3, 11]);
        assert_eq!(choice.cpu_list.iter().filter(|&&cpu| cpu < 8).count(), 4, "4 in each domain");
    }

    /// The m8a.48xlarge: 2 NUMA nodes of 96 cores, 12 L3 groups of 8 in each, every priority 0. Each round takes one core of every group,
    /// the nodes alternating, so half the cores is 4 in every group and 48 in each node, and 30 threads split 15 and 15.
    #[test]
    fn two_nodes_of_many_l3_groups_balance_both() {
        let cores = synthetic(&[0; 192], 8, 96, false);
        let choice = pick(&cores, "1", "50%", None, true);
        assert_eq!(&choice.order[..6], [0, 96, 8, 104, 16, 112]);
        assert_eq!(choice.skipped, [191], "the last of the last round");
        assert_eq!(choice.cpu_list.len(), 96);
        for group in 0..24 {
            assert_eq!(choice.cpu_list.iter().filter(|&&cpu| cpu / 8 == group).count(), 4, "L3 group {group}");
        }
        assert_eq!(choice.cpu_list.iter().filter(|&&cpu| cpu < 96).count(), 48, "node 0");
        let thirty = pick(&cores, "1", "30", None, true);
        assert_eq!(thirty.cpu_list.iter().filter(|&&cpu| cpu < 96).count(), 15, "30 threads, node 0");
    }

    /// The L3 index is group-relative (Windows' CPU sets): the same index in two processor groups is two L3s.
    #[test]
    fn the_l3_key_carries_the_processor_group() {
        let mut cores = synthetic(&[0; 4], 4, 4, false);
        for (core, group) in cores.iter_mut().zip([0u16, 0, 1, 1]) {
            core.l3 = (group, 0);
        }
        assert_eq!(rank(&cores), [0, 2, 1, 3]);
    }

    /// With core types, performance cores come first, then efficiency cores (Meteor Lake's E-cores before its LP E-cores), then unknown
    /// ones; within each type the priority ranks them. Zen 5 and Zen 5c on their own L3s (a Strix Point) don't interleave the types.
    #[test]
    fn core_types_order_before_priority() {
        let cores = intel_hybrid();
        let order = rank(&cores).into_iter().map(|i| cores[i].id).collect::<Vec<_>>();
        assert_eq!(&order[..8], [2, 5, 0, 1, 3, 4, 6, 7]);
        assert_eq!(&order[8..], (8..24).collect::<Vec<_>>());

        let mut strix = synthetic(&[0; 12], 12, 12, true);
        for core in &mut strix[4..] {
            core.l3 = (0, 1);
        }
        assert_eq!(rank(&strix), [0, 4, 1, 5, 2, 6, 3, 7, 8, 9, 10, 11], "untyped, the two L3s take turns");
        for core in &mut strix[4..] {
            core.core_type = E(0);
        }
        assert_eq!(rank(&strix), (0..12).collect::<Vec<_>>(), "typed, the Zen 5 cores first");

        let mut meteor = synthetic(&[0; 4], 4, 4, false);
        for (core, kind) in meteor.iter_mut().zip([E(0), P(0), CoreType::Unknown, E(1)]) {
            core.core_type = kind;
        }
        assert_eq!(rank(&meteor), [1, 3, 0, 2]);
    }

    /// The 8500G with core-priority=hybrid: the labels change, the order and the choice don't (TODO 108).
    #[test]
    fn core_priority_hybrid_only_labels() {
        let equal = synthetic(&R8500G, 64, 64, true);
        let mut hybrid = equal.clone();
        for core in &mut hybrid {
            core.core_type = if core.priority == 1 { P(0) } else { E(0) };
        }
        for (skip, cpus, cputype_cores) in [("1", "100%", false), ("1", "50%", true), ("2", "3", true), ("0", "100%", false)] {
            assert_eq!(pick(&equal, skip, cpus, None, cputype_cores), pick(&hybrid, skip, cpus, None, cputype_cores), "{skip} {cpus}");
        }
        assert_eq!(pick(&hybrid, "1", "100%", None, true).skipped, [5], "a Zen 4c core");
    }

    // ---- skip-cores= ----

    #[test]
    fn skip_specs_parse() {
        assert_eq!(SkipSpec::parse("0"), Ok(SkipSpec::Count(0)));
        assert_eq!(SkipSpec::parse(" 4 "), Ok(SkipSpec::Count(4)));
        assert_eq!(SkipSpec::parse("25%"), Ok(SkipSpec::Percent(25)));
        assert_eq!(SkipSpec::parse("0-7"), Ok(SkipSpec::Range(0, 7)));
        assert!(SkipSpec::parse("101%").unwrap_err().contains("<= 100"));
        assert!(SkipSpec::parse("7-0").unwrap_err().contains("start 7 > end 0"));
        assert!(SkipSpec::parse("x").unwrap_err().contains("expected N, N%, or A-B"));
        for spec in ["3", "25%", "8-15"] {
            assert_eq!(SkipSpec::parse(spec).unwrap().to_string(), spec);
        }
    }

    /// `N` and `N%` take the weakest cores, from the end of the order; `A-B` takes core numbers wherever they rank. On the 8600G the
    /// default leaves out core 4 (priority 0), not core 0.
    #[test]
    fn skip_takes_the_end_of_the_order_or_a_range() {
        let cores = synthetic(&R8600G, 64, 64, true);
        assert_eq!(pick(&cores, "1", "100%", None, true).skipped, [4]);
        assert_eq!(pick(&cores, "2", "100%", None, true).skipped, [2, 4]);
        assert_eq!(pick(&cores, "0", "100%", None, true).skipped, [] as [usize; 0]);
        assert_eq!(pick(&cores, "50%", "100%", None, true).skipped, [1, 2, 4]);
        assert_eq!(pick(&cores, "100%", "100%", None, true).skipped, [0, 1, 2, 4, 5], "never every core: core 3 stays");
        let range = pick(&cores, "3-5", "100%", None, true);
        assert_eq!(range.skipped, [3, 4, 5]);
        assert_eq!(cores_of(&cores, &range.cpu_list), [0, 1, 2]);
        assert_eq!(pick(&synthetic(&R5700X, 64, 64, true), "1", "100%", None, true).skipped, [6]);
    }

    /// Bug 1 (TODO 108): skip-cores=50% or more under cputype=threads on 2-way SMT, and skip-cores=N with N at least the core count,
    /// left an empty CPU list. Now a share leaves at least one core, and a count or range that leaves none stops with an error.
    #[test]
    fn a_skip_never_leaves_an_empty_list() {
        let cores = synthetic(&R8600G, 64, 64, true);
        for skip in ["50%", "75%", "100%"] {
            let choice = pick(&cores, skip, "100%", None, false);
            assert!(!choice.cpu_list.is_empty(), "skip-cores={skip}");
        }
        for skip in ["6", "7", "1000"] {
            let e = choose(&cores, &request(skip, "100%", None, false)).unwrap_err();
            assert!(e.starts_with(&format!("skip-cores={skip} leaves no core")) && e.contains("this CPU has 6"), "{e}");
        }
        let e = choose(&cores, &request("0-5", "100%", None, true)).unwrap_err();
        assert_eq!(e, "skip-cores=0-5 leaves no core to test: this CPU's are cores 0-5");
        assert_eq!(pick(&cores, "5", "100%", None, false).cpu_list, [6, 7], "one core left, both its CPUs");
    }

    /// Bug 2 (TODO 108): `N%` is a share of the cores under either cputype: 25% of 6 cores (12 threads) is 2 cores, not 3.
    #[test]
    fn a_skip_share_counts_cores_not_cpus() {
        let cores = synthetic(&R8600G, 64, 64, true);
        for cputype_cores in [false, true] {
            let choice = pick(&cores, "25%", "100%", None, cputype_cores);
            assert_eq!(choice.skipped.len(), 2, "cputype=cores {cputype_cores}");
        }
        assert_eq!(pick(&synthetic(&[0; 4], 64, 64, false), "25%", "100%", None, true).skipped, [3]);
        assert_eq!(pick(&synthetic(&[0; 16], 64, 64, false), "25%", "100%", None, true).skipped, [12, 13, 14, 15]);
    }

    // ---- cpus= ----

    /// `cpus=` from the front of what's left: a share rounded to the nearest, or a count; `0` and `0%` one thread, over 100% (or more
    /// than there are) every one, and no share overflows (2026-11-02: `available * percent` in u32).
    #[test]
    fn cpus_takes_a_share_or_a_count_and_never_overflows() {
        assert_eq!(thread_count("50%", 15), (8, Some(50)));
        assert_eq!(thread_count("50%", 5), (3, Some(50)));
        assert_eq!(thread_count("0%", 10), (1, Some(0)));
        assert_eq!(thread_count("0", 10), (1, None));
        assert_eq!(thread_count("4", 10), (4, None));
        assert_eq!(thread_count("150%", 10), (10, Some(150)));
        assert_eq!(thread_count("1000000", 10), (10, None));
        assert_eq!(thread_count("4294967295%", 192), (192, Some(u32::MAX)));
        assert_eq!(thread_count("4294967295%", usize::MAX), (usize::MAX, Some(u32::MAX)));

        let cores = synthetic(&R8600G, 64, 64, true);
        assert_eq!(pick(&cores, "1", "1", None, true).cpu_list, [6], "--cache-latency's one CPU: the top-ranked core's");
        assert_eq!(pick(&cores, "1", "4294967295%", None, false).cpu_list.len(), 10);
        assert_eq!(pick(&cores, "1", "0%", None, false).cpu_list.len(), 1);
    }

    /// cputype=threads takes every chosen core's first CPU before any second, in the order; cputype=cores one a core. SMT off, both are
    /// the same.
    #[test]
    fn threads_pair_siblings_after_the_first_cpus() {
        let cores = synthetic(&R8600G, 64, 64, true);
        let threads = pick(&cores, "1", "100%", None, false);
        assert_eq!(threads.available, 10);
        assert_eq!(threads.cpu_list, [6, 10, 0, 2, 4, 7, 11, 1, 3, 5]);
        let seven = pick(&cores, "1", "7", None, false);
        assert_eq!(seven.cpu_list, [6, 10, 0, 2, 4, 7, 11]);
        let by_core = pick(&cores, "1", "100%", None, true);
        assert_eq!((by_core.available, by_core.cpu_list), (5, vec![6, 10, 0, 2, 4]));

        let no_smt = synthetic(&R8600G, 64, 64, false);
        assert_eq!(pick(&no_smt, "1", "100%", None, false).cpu_list, pick(&no_smt, "1", "100%", None, true).cpu_list);
        assert_eq!(pick(&no_smt, "1", "100%", None, false).cpu_list, [3, 5, 0, 1, 2]);
    }

    /// Bug 5 (TODO 108): cputype=threads halved the cache-sized extents even when the choice put one thread a core (`cpus=50%`). The
    /// threads a core is now the choice's.
    #[test]
    fn threads_a_core_is_what_the_choice_puts_on_one() {
        let cores = synthetic(&R8600G, 64, 64, true);
        assert_eq!(pick(&cores, "1", "50%", None, false).threads_per_core, 1);
        assert_eq!(pick(&cores, "1", "100%", None, false).threads_per_core, 2);
        assert_eq!(pick(&cores, "1", "6", None, false).threads_per_core, 2, "one core with both");
        assert_eq!(pick(&cores, "1", "100%", None, true).threads_per_core, 1);
        assert_eq!(pick(&synthetic(&R8600G, 64, 64, false), "1", "100%", None, false).threads_per_core, 1);
    }

    // ---- cpu-stride= ----

    /// Given, `cpu-stride=` takes what's left by core number, as before TODO 108, and spaces the picks; skip-cores=N still leaves out the
    /// weakest. Without it the ranked order is the default.
    #[test]
    fn a_stride_spaces_the_cores_by_number() {
        let cores = synthetic(&[0; 16], 8, 16, false);
        assert_eq!(pick(&cores, "0", "50%", Some("even"), true).cpu_list, [0, 2, 4, 6, 8, 10, 12, 14]);
        assert_eq!(pick(&cores, "0", "4", Some("2"), true).cpu_list, [0, 2, 4, 6]);
        assert_eq!(pick(&cores, "0", "4", Some("1"), true).cpu_list, [0, 1, 2, 3], "packed by number");
        let r8600g = synthetic(&R8600G, 64, 64, false);
        assert_eq!(pick(&r8600g, "1", "3", Some("2"), true).cpu_list, [0, 2, 5], "core 4, the weakest, left out");
        let e = choose(&cores, &request("0", "8", Some("4"), true)).unwrap_err();
        assert!(e.contains("cannot place 8 threads"), "{e}");
    }

    /// Bug 3 (TODO 108): with core types, `cpu-stride=` was ignored without a word. It now spaces the cores by number the same way.
    #[test]
    fn a_stride_applies_with_core_types() {
        let cores = intel_hybrid();
        let strided = pick(&cores, "0", "4", Some("2"), true);
        assert_eq!(cores_of(&cores, &strided.cpu_list), [0, 2, 4, 6]);
        let ranked = pick(&cores, "0", "4", None, true);
        assert_eq!(cores_of(&cores, &ranked.cpu_list), [2, 5, 0, 1]);
        assert_eq!(pick(&cores, "1", "100%", None, true).skipped, [23], "the last E-core");
    }

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

    // ---- The start-up table ----

    /// Bug 4 (TODO 108): the table showed the cores skip-cores=A-B drops as Available. Its statuses now come from the choice.
    #[test]
    fn the_table_marks_a_skipped_range() {
        let cores = synthetic(&[0; 16], 8, 16, false);
        let asked = request("0-7", "100%", None, true);
        let choice = choose(&cores, &asked).unwrap();
        let (report, ..) = startup_topology(&cores, &asked, &choice, true, false);
        let status = |cpu: usize| report.cpus[cpu].status.as_str();
        assert!((0..8).all(|cpu| status(cpu) == "Skipped"), "{:?}", report.cpus);
        assert!((8..16).all(|cpu| status(cpu) == "Assigned"));
        assert_eq!((report.summary.skipped_count, report.summary.available_count), (8, 0));
    }

    /// Bug 6 (TODO 108): the skip order was written three times, the count, the selection and the table, already out of step. They come
    /// from one choice: the CPUs available are those on the cores left, the table skips the choice's cores and assigns its CPUs, each
    /// thread its own.
    #[test]
    fn the_count_the_choice_and_the_table_agree() {
        let mut hybrid = synthetic(&R8500G, 64, 64, true);
        for core in &mut hybrid {
            core.core_type = if core.priority == 1 { P(0) } else { E(0) };
        }
        let machines = [
            ("8600G", synthetic(&R8600G, 64, 64, true)),
            ("5700X, SMT off", synthetic(&R5700X, 64, 64, false)),
            ("8500G hybrid", hybrid),
            ("m8a.4xlarge", synthetic(&[0; 16], 8, 16, false)),
            ("Intel hybrid", intel_hybrid()),
        ];
        let specs = [("1", "100%"), ("0", "50%"), ("2", "3"), ("25%", "100%"), ("50%", "50%"), ("1-2", "100%"), ("1", "0")];
        for (name, cores) in &machines {
            for (skip, cpus) in specs {
                for (stride, cputype_cores) in [(None, false), (None, true), (Some("1"), true)] {
                    let asked = request(skip, cpus, stride, cputype_cores);
                    let choice = choose(cores, &asked).unwrap();
                    let case = format!("{name}: skip-cores={skip} cpus={cpus} cpu-stride={stride:?} cores={cputype_cores}");
                    let left: Vec<&Core> = cores.iter().filter(|core| !choice.skipped.contains(&core.id)).collect();
                    let left_cpus: usize = left.iter().map(|core| core.cpus.len()).sum();
                    assert_eq!(choice.available, if cputype_cores { left.len() } else { left_cpus }, "{case}");
                    let (report, ..) = startup_topology(cores, &asked, &choice, true, false);
                    for entry in &report.cpus {
                        let skipped = choice.skipped.contains(&entry.physical_core);
                        assert_eq!(entry.status == "Skipped", skipped, "{case}: CPU {}", entry.logical_cpu);
                        let thread = choice.cpu_list.iter().position(|&cpu| cpu == entry.logical_cpu);
                        assert_eq!(entry.thread_id, thread, "{case}");
                        assert!(!(skipped && thread.is_some()), "{case}: a skipped core's CPU {} has a thread", entry.logical_cpu);
                    }
                    assert_eq!(report.summary.assigned_count, choice.cpu_list.len(), "{case}");
                    let unique: HashSet<usize> = choice.cpu_list.iter().copied().collect();
                    assert_eq!(unique.len(), choice.cpu_list.len(), "{case}: a CPU twice");
                }
            }
        }
    }

    /// The table's priority and L3 group columns, and its hybrid notes: P-cores with SMT, and E-cores skipped first only for a skip from
    /// the end.
    #[test]
    fn the_table_shows_each_cores_priority_and_l3_group() {
        let cores = synthetic(&[0, 0, 1, 0], 2, 4, true);
        let asked = request("1", "100%", None, false);
        let choice = choose(&cores, &asked).unwrap();
        let (report, smt, e_first) = startup_topology(&cores, &asked, &choice, true, false);
        let columns: Vec<(usize, u8, usize)> = report.cpus.iter().map(|e| (e.logical_cpu, e.priority, e.l3_group)).collect();
        assert_eq!(columns, [(0, 0, 0), (1, 0, 0), (2, 0, 0), (3, 0, 0), (4, 1, 1), (5, 1, 1), (6, 0, 1), (7, 0, 1)]);
        assert_eq!((smt, e_first), (None, false));

        let cores = intel_hybrid();
        for (skip, e_first) in [("1", true), ("0", false), ("8-9", false)] {
            let asked = request(skip, "100%", None, true);
            let choice = choose(&cores, &asked).unwrap();
            let (_, smt, skipped_first) = startup_topology(&cores, &asked, &choice, true, true);
            assert_eq!((smt, skipped_first), (Some(8), e_first), "skip-cores={skip}");
        }
    }

    #[test]
    fn core_lists_read_as_words() {
        assert_eq!(core_list(&[3]), "core 3");
        assert_eq!(core_list(&[4, 0]), "cores 0 and 4");
        assert_eq!(core_list(&[0, 2, 4]), "cores 0, 2 and 4");
        assert_eq!(core_list(&[0, 1]), "cores 0 and 1");
        assert_eq!(core_list(&(0..8).collect::<Vec<_>>()), "cores 0-7");
        assert_eq!(core_list(&[0, 1, 2, 3, 8]), "cores 0-3 and 8");
        assert_eq!(core_list(&[]), "no core");
    }
}
