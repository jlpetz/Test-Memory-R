//! Built-in test plans (TODO 25): which of the built-in suite's tests a run takes, and in what
//! order. A plan only chooses. Each test keeps its built-in settings (extent, chunk, timing,
//! program), so `plan=stability` and `test=Mem-MovingInv` run the same Mem-MovingInv, and
//! tuning a test in the suite tunes it in every plan. `config=` brings its own tests instead.
//!
//! Names are the suite's registration names; an `-Auto` name runs the widest width the CPU has.
//! The layout and the reasons for each test: `doc/todo/25-curated-plans.md`.

use crate::runner::TestDefinition;

pub struct Plan {
    pub name: &'static str,
    /// For the help and the startup line.
    pub about: &'static str,
    /// In run order; `None` is the whole suite, in its own order.
    pub tests: Option<&'static [&'static str]>,
    /// Cycles when the command line gives none.
    pub cycles: Option<u32>,
}

/// What a run with no `plan=`, `config=` or test filter runs.
pub const DEFAULT: &str = "stability";

/// The stability families at the widest width: a hard-fault screen first, then the full-memory
/// pattern tests, then the access-shape ones. CacheBust is left out (it works mostly inside the
/// caches), as are the flush variants (at these chunk sizes eviction already reaches DRAM).
const STABILITY: &[&str] = &[
    "Mem-StuckBitAuto",
    "Mem-SimpleV2-Auto",
    "Mem-SimpleNT-Auto",
    "Mem-MovingInv",
    "Mem-ModuloX",
    "Mem-Sequence",
    "Mem-Random",
    "Mem-RefreshAuto",
    "Mem-MirrorV2-Auto",
    "Mem-Stride",
    "Mem-BlockMove",
];

/// The families with width variants, every width side by side: one run, so every width tests
/// the same memory in the same few seconds of each cycle.
const WIDTHS: &[&str] = &[
    "Mem-StuckBit", "Mem-StuckBit128", "Mem-StuckBit256", "Mem-StuckBit512", "Mem-StuckBitAuto",
    "Mem-SimpleNT-128", "Mem-SimpleNT-256", "Mem-SimpleNT-512", "Mem-SimpleNT-Auto",
    "Mem-Refresh", "Mem-Refresh128", "Mem-Refresh256", "Mem-Refresh512", "Mem-RefreshAuto",
    "Mem-SimpleV2", "Mem-SimpleV2-128", "Mem-SimpleV2-256", "Mem-SimpleV2-512", "Mem-SimpleV2-Auto",
    "Mem-MirrorV2", "Mem-MirrorV2-128", "Mem-MirrorV2-256", "Mem-MirrorV2-512", "Mem-MirrorV2-Auto",
];

/// Unloaded latency (one dependent access at a time, the figure other tools report), bandwidth,
/// and the write that skips the cache.
const PERF: &[&str] = &[
    "Lat-L1-Read", "Lat-L1-Write", "Lat-L1-Copy",
    "Lat-L2-Read", "Lat-L2-Write", "Lat-L2-Copy",
    "Lat-L3-Read", "Lat-L3-Write", "Lat-L3-Copy",
    "Lat-DRAM-Read", "Lat-DRAM-Write", "Lat-DRAM-Copy",
    "Lat-DRAMFull-Read", "Lat-DRAMFull-Write", "Lat-DRAMFull-Copy",
    "Spd-L1-Read-Auto", "Spd-L1-Write-Auto", "Spd-L1-Copy-Auto",
    "Spd-L2-Read-Auto", "Spd-L2-Write-Auto", "Spd-L2-Copy-Auto",
    "Spd-L3-Read-Auto", "Spd-L3-Write-Auto", "Spd-L3-Copy-Auto",
    "Spd-DRAMSmall-Read-Auto", "Spd-DRAMSmall-Write-Auto", "Spd-DRAMSmall-Copy-Auto",
    "Spd-DRAMFull-Read-Auto", "Spd-DRAMFull-Write-Auto", "Spd-DRAMFull-Copy-Auto",
    "Lat-NTW-DRAM-Write-Auto",
];

pub const PLANS: &[Plan] = &[
    Plan { name: "quick", about: "the stability tests once", tests: Some(STABILITY), cycles: Some(1) },
    Plan { name: "stability", about: "the stability tests at the widest width", tests: Some(STABILITY), cycles: None },
    Plan { name: "widths", about: "every width of the tests that have them, side by side", tests: Some(WIDTHS), cycles: None },
    Plan { name: "perf", about: "latency and bandwidth", tests: Some(PERF), cycles: Some(1) },
    Plan { name: "all", about: "every test in the suite", tests: None, cycles: None },
];

/// The plan called `name`, or an error naming the plans there are.
pub fn find(name: &str) -> Result<&'static Plan, String> {
    PLANS.iter().find(|p| p.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
        let names: Vec<&str> = PLANS.iter().map(|p| p.name).collect();
        format!("Unknown plan '{name}': the plans are {}", names.join(", "))
    })
}

/// The plan that picks a run's tests: the one named, else the default. `None` when a config
/// brings its own tests, or a test filter with no plan picks from every test, as it always has.
pub fn resolve(named: Option<&'static Plan>, has_config: bool, has_filter: bool) -> Result<Option<&'static Plan>, String> {
    match (named, has_config) {
        (Some(plan), true) => Err(format!("plan={} and config= both choose the tests: give one", plan.name)),
        (Some(plan), false) => Ok(Some(plan)),
        (None, false) if !has_filter => find(DEFAULT).map(Some),
        (None, _) => Ok(None),
    }
}

/// A 512-bit variant on a CPU without AVX-512: it would only return empty results.
fn unsupported(name: &str) -> bool {
    name.contains("512") && !name.ends_with("Auto") && !is_x86_feature_detected!("avx512f")
}

/// The plan's tests from the built-in `suite`, in the plan's order, numbered as its tests.
/// A 512-bit variant the CPU can't run is left out (logged); any other name must be in the suite.
pub fn select(plan: &Plan, suite: Vec<TestDefinition>) -> Result<Vec<TestDefinition>, String> {
    let Some(names) = plan.tests else {
        return Ok(suite);
    };
    let mut suite: Vec<Option<TestDefinition>> = suite.into_iter().map(Some).collect();
    let mut tests = Vec::with_capacity(names.len());
    for &name in names {
        if unsupported(name) {
            log::info!("plan {}: {name} left out, the CPU has no AVX-512", plan.name);
            continue;
        }
        let at = suite
            .iter()
            .position(|d| d.as_ref().is_some_and(|d| d.actual_name == name || d.original_name == Some(name)))
            .ok_or_else(|| format!("plan {}: bug: no test {name} in the suite", plan.name))?;
        let mut def = suite[at].take().expect("found above");
        def.test_index = tests.len();
        tests.push(def);
    }
    Ok(tests)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite() -> Vec<TestDefinition> {
        crate::runner::create_test_definitions(crate::tests::get_cache_info())
    }

    /// Every name in every plan is a test in the suite, once.
    #[test]
    fn every_plan_names_suite_tests() {
        for plan in PLANS {
            let tests = select(plan, suite()).unwrap_or_else(|e| panic!("{e}"));
            let expected = plan.tests.map_or(suite().len(), |names| names.iter().filter(|n| !unsupported(n)).count());
            assert_eq!(tests.len(), expected, "plan {}", plan.name);
            assert!(tests.iter().enumerate().all(|(i, t)| plan.tests.is_none() || t.test_index == i));
        }
    }

    /// The plans run in their own order, not the suite's: StuckBit leads stability.
    #[test]
    fn plans_keep_their_order() {
        let tests = select(find("stability").unwrap(), suite()).unwrap();
        let names: Vec<&str> = tests.iter().map(|t| t.original_name.unwrap_or(t.actual_name)).collect();
        assert_eq!(names, STABILITY);
        assert!(!names.iter().any(|n| n.contains("CacheBust") || n.contains("Flush")));
    }

    /// A plain run takes the default; a filter alone picks from every test, as before plans;
    /// a config brings its own tests and can't take a plan as well.
    #[test]
    fn the_plan_a_run_takes() {
        let quick = find("quick").unwrap();
        let name = |r: Result<Option<&Plan>, String>| r.unwrap().map(|p| p.name);
        assert_eq!(name(resolve(None, false, false)), Some("stability"));
        assert_eq!(name(resolve(None, false, true)), None);
        assert_eq!(name(resolve(None, true, false)), None);
        assert_eq!(name(resolve(Some(quick), false, true)), Some("quick"));
        assert!(resolve(Some(quick), true, false).is_err());
    }

    #[test]
    fn names_are_found_without_case_and_unknown_ones_are_listed() {
        assert_eq!(find("Quick").unwrap().name, "quick");
        assert_eq!(find(DEFAULT).unwrap().name, "stability");
        let err = find("soak").err().unwrap();
        assert!(err.contains("quick, stability, widths, perf, all"), "{err}");
    }
}
