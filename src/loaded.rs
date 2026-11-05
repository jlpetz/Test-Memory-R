//! The loaded latency tests (TODO 117): a read's latency while the other cores load DRAM, which no unloaded test sees. On daddy-petz
//! (`doc/todo/117-write-latency.md`) tWR 12 to 24 moved one figure, a read's latency under a flat-out NT-write load (+6.7 ns), and
//! tRDWR 8 to 12 one other, the turnaround at matched load (+12 ns at 50%); the idle figures and every other Lat-* test moved with
//! neither. Lat-Loaded-DRAM measures a read's latency idle, under flat-out NT writes (its headline) and under flat-out reads.
//! Lat-Turnaround-DRAM, the same function's other registration, adds matched levels: the loaders held to 25, 50 and 75% of the lower
//! flat-out peak (or MiB/s pinned, `load_levels`), writing and then reading at the same MiB/s, its turnaround the latency under writes
//! less that under reads, which is what writes cost a read beyond the bus time they take.
//!
//! **Roles**, chosen once from the step's CPUs (`assign_roles`): thread 0, the run's top-ranked core, measures; the first thread of each
//! other physical core on its node loads; the rest sit out (thread 0's SMT siblings, the loaders' siblings, other nodes' threads) and
//! return at once, so the measurer's core has no busy sibling, other nodes' controllers stay idle and cputype= doesn't change the loader
//! set. They share one `Step` (`TestMemoryConfig::loaded`, built by `ThreadPool::execute_test`), as the storm's and the exchange's do.
//!
//! **The measurer** chains one cell per 128 B line pair over its extent (`build_cycle_strided`): each cell comes round once a cycle, the
//! adjacent-line prefetcher never fetches one, and over 32 L3s or more none hits the L3 (Lat-DRAM-Read's 8 B slots over 4 L3s hit it
//! about a quarter of the time on one thread, and a read load takes those hits away where NT writes don't). It drives the schedule
//! through the phase word: each window it sets the loaders' load, chases untimed for the settle, then times samples of 1000 chained reads
//! (`latency_tests::chase`) until the window's TSC end. A window's ns is its timed ticks over its reads, its MiB/s the loaders' bytes
//! over exactly the timed span, from their slots. It counts no bytes, so the step's MiB/s is the loaders'.
//!
//! **A loader** fills its extent once (NT), then per 4 KiB block calls its kernel (`bandwidth_tests::load_kernels`, at the run's width),
//! stores its running byte count to its own slot and reads the phase word. Each window it restarts at a random block, so a window is a
//! new instance of the load: the probe's windows sat within 1 ns of each other while two runs' differed by 12. Parked (an idle window),
//! it spins with PAUSE in C0, so the idle rows run at the loaded rows' clocks. Under a level it is held to its share (the level over
//! the loaders) by a TSC deadline a block (`pace`): staggered, loader i's first i/L of an interval after the first's, so the blocks
//! interleave; its debt capped at an interval, so it never bursts to catch up; and each block later than that counted as fallen behind.
//! The levels come from this step's own flat-out pairs, which run first (`resolve`).
//!
//! **The schedule** is ABBA pairs (W R, R W, ...) between two idle windows, so linear drift cancels within a pair. Every raw window is
//! kept, and `summarize` turns them into rows: a pure function, which can be changed and rerun on results already taken. The design and
//! its reasons: `doc/todo/117-loaded-latency-design.md`.

use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ErrorMode;
use crate::latency_histogram::Histogram;
use crate::latency_tests::LatencyTestStats;
use crate::runner::AllocationBlock;
use crate::test_scaffolding::TestRunner;
use crate::tests::{Stage, TestAction, TestBlock, TestMemoryConfig, TestProgress, TestTiming};
use crate::thread_pool::CpuAssignment;

pub const LOADED_NAME: &str = "Lat-Loaded-DRAM";
pub const TURNAROUND_NAME: &str = "Lat-Turnaround-DRAM";
/// The loaded latency tests, a schedule each
pub const NAMES: [&str; 2] = [LOADED_NAME, TURNAROUND_NAME];

/// Whether `name` is one of the loaded latency tests
pub fn is_loaded(name: &str) -> bool {
    NAMES.contains(&name)
}

/// A loader's block: one kernel call, its byte count stored and the phase word read after each
pub const BLOCK: usize = 4096;
/// The measurer's chain cell: a 128 B line pair, so the adjacent-line prefetcher never brings in another cell
pub const CELL: usize = 128;
/// Each window's untimed chase before its timed one, while the loaders' new load settles (no longer than the window)
pub const SETTLE: Duration = Duration::from_millis(200);
/// How near its target a level's reached MiB/s must be, each kind's, for its row to be matched
pub const MATCH: f64 = 0.03;
/// A window's length when the config gives none, and the most it may give
pub const DEFAULT_WINDOW_MS: u32 = 1000;
pub const MAX_WINDOW_MS: u32 = 60_000;
/// The fewest a config may give: a shorter window is mostly its settle's tail and a few samples (unit tests set less directly)
pub const MIN_WINDOW_MS: u32 = 10;
/// Lat-Loaded-DRAM's ABBA pairs flat out when the config gives none, Lat-Turnaround-DRAM's at each load, and the most either may give
pub const LOADED_PAIRS: u32 = 3;
pub const TURNAROUND_PAIRS: u32 = 5;
pub const MAX_PAIRS: u32 = 50;
/// Lat-Turnaround-DRAM's levels when the config gives none: on daddy-petz tRDWR 8 to 12 moved the turnaround +12 ns at 50% and +24 at
/// 75%, and 25% is the near-idle end (`doc/todo/117-write-latency.md`)
pub const DEFAULT_LEVELS: [Level; 3] = [Level::Percent(25.0), Level::Percent(50.0), Level::Percent(75.0)];
/// The most levels a step may take: each adds its pairs' windows
pub const MAX_LEVELS: usize = 8;
/// A pinned level's range, MiB/s: the loaders held to 1 MiB/s between them up to about 1 TB/s, past any controller's
pub const MIN_LEVEL_MIB_S: f64 = 1.0;
pub const MAX_LEVEL_MIB_S: f64 = 1_000_000.0;
/// Chained reads a sample, as the other Lat-* tests take
const READS: usize = 1000;
/// A loader's blocks between its progress and its stop check: 1 MiB
const BLOCKS_A_CHECK: usize = 256;
const MIB: f64 = (1u64 << 20) as f64;

/// The extent the loaded tests register with: 32 of the CPU's L3 (one instance, CPUID's), 1 GiB at least, so the chain never hits the
/// L3 and its size doesn't move with memory= or the threads (1 GiB on a 5700X or an 8600G, 3 GiB on an X3D). A smaller span caps it.
pub fn extent_bytes(l3_bytes: usize) -> usize {
    (1usize << 30).max(32 * l3_bytes)
}

/// What a thread does in a loaded step, chosen once (`assign_roles`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Thread 0: chases its chain and times it, and drives the schedule
    Measures,
    /// Streams over its extent at the window's load; `index` is its byte slot
    Loads { index: usize },
    /// Returns at once
    SitsOut,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Measures => "measures",
            Role::Loads { .. } => "loads",
            Role::SitsOut => "sits out",
        }
    }
}

/// Each thread's role, by thread id, from the step's CPUs (`(thread, cpu, node)`, `ThreadPool::cpu_assignments`) and `core_of` (a CPU's
/// physical core): thread 0, the run's top-ranked core, measures; the first thread (by id) of each other physical core on its node loads,
/// its index the order it was found in; every other thread sits out. No threads is thread 0 alone.
pub fn assign_roles(assignments: &[CpuAssignment], core_of: impl Fn(usize) -> usize) -> Vec<Role> {
    let threads = assignments.iter().map(|&(t, _, _)| t + 1).max().unwrap_or(1);
    let mut roles = vec![Role::SitsOut; threads];
    roles[0] = Role::Measures;
    let Some(&(_, cpu, node)) = assignments.iter().find(|&&(t, _, _)| t == 0) else {
        return roles;
    };
    let mut by_thread = assignments.to_vec();
    by_thread.sort_by_key(|&(t, _, _)| t);
    let mut cores = vec![core_of(cpu)];
    for (t, cpu, n) in by_thread {
        let core = core_of(cpu);
        if t != 0 && n == node && !cores.contains(&core) {
            roles[t] = Role::Loads { index: cores.len() - 1 };
            cores.push(core);
        }
    }
    roles
}

/// The loaders among `roles`
fn loaders_of(roles: &[Role]) -> usize {
    roles.iter().filter(|r| matches!(r, Role::Loads { .. })).count()
}

/// What the loaders do in a window: the low bits of the phase word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An idle window: the loaders spin on the phase word with PAUSE, moving no memory
    #[serde(rename = "idle")]
    Park = 0,
    /// NT writes, a block at a time, each fenced
    Writes = 1,
    /// Reads
    Reads = 2,
    /// The schedule is over: the loaders finish
    Done = 3,
}

/// The phase word: the window's number above the kind, so a window of the same kind as the last is a new one too
fn phase_word(window: u64, kind: Kind) -> u64 {
    (window << 2) | kind as u64
}

fn kind_of(word: u64) -> Kind {
    match word & 3 {
        0 => Kind::Park,
        1 => Kind::Writes,
        2 => Kind::Reads,
        _ => Kind::Done,
    }
}

/// One window of a schedule: its load, its level (`None` idle and flat out) and its ABBA pair (`None` idle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub kind: Kind,
    pub level: Option<usize>,
    pub pair: Option<usize>,
}

impl Window {
    const IDLE: Window = Window { kind: Kind::Park, level: None, pair: None };

    /// Its histogram among the schedule's (`Schedule::cells`): idle 0, flat out 1 (writes) and 2 (reads), level `l` 3 + 2l and 4 + 2l
    fn cell(self) -> usize {
        let reads = usize::from(self.kind == Kind::Reads);
        match (self.kind, self.level) {
            (Kind::Park | Kind::Done, _) => 0,
            (_, None) => 1 + reads,
            (_, Some(level)) => 3 + 2 * level + reads,
        }
    }
}

/// A step's windows in order, and how long each is.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub windows: Vec<Window>,
    pub levels: usize,
    pub window: Duration,
    pub settle: Duration,
}

impl Schedule {
    /// An idle window, `pairs` ABBA pairs flat out, `pairs` at each of `levels` levels, and an idle window: a pair is W R, the next R W,
    /// and so on, so linear drift cancels over two
    pub fn new(pairs: usize, levels: usize, window: Duration, settle: Duration) -> Self {
        let mut windows = vec![Window::IDLE];
        for level in std::iter::once(None).chain((0..levels).map(Some)) {
            for pair in 0..pairs {
                let order = if pair % 2 == 0 { [Kind::Writes, Kind::Reads] } else { [Kind::Reads, Kind::Writes] };
                windows.extend(order.map(|kind| Window { kind, level, pair: Some(pair) }));
            }
        }
        windows.push(Window::IDLE);
        Schedule { windows, levels, window, settle }
    }

    /// With no loaders: the two idle windows alone
    pub fn idle(window: Duration, settle: Duration) -> Self {
        Schedule { windows: vec![Window::IDLE; 2], levels: 0, window, settle }
    }

    /// Its histograms, a row's kind each (`Window::cell`)
    pub fn cells(&self) -> usize {
        3 + 2 * self.levels
    }

    /// How long it takes, its settles in: the step's length, as the ticker's estimate
    pub fn expected_secs(&self) -> f64 {
        self.windows.len() as f64 * (self.window + self.settle).as_secs_f64()
    }
}

/// A matched level (Lat-Turnaround-DRAM's `load_levels`): a share of the lower flat-out peak, `25%`, or MiB/s pinned, `8058`, so two
/// settings can be compared at the same load (daddy-petz's overclock moved its 50% from 8.4 to 14.1 GB/s). Written as given in the
/// results and on the command line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Level {
    Percent(f64),
    MiBs(f64),
}

impl Level {
    /// One level: `N%`, more than 0 and at most 100, or MiB/s, `MIN_LEVEL_MIB_S` to `MAX_LEVEL_MIB_S`
    pub fn parse(s: &str) -> Result<Level, String> {
        let s = s.trim();
        let number = |n: &str| n.trim().parse::<f64>().ok().filter(|v| v.is_finite());
        match s.strip_suffix('%') {
            Some(share) => match number(share) {
                Some(p) if p > 0.0 && p <= 100.0 => Ok(Level::Percent(p)),
                _ => Err(format!("level '{s}': a share of the lower flat-out peak is more than 0% and at most 100%")),
            },
            None => match number(s) {
                Some(m) if (MIN_LEVEL_MIB_S..=MAX_LEVEL_MIB_S).contains(&m) => Ok(Level::MiBs(m)),
                _ => Err(format!(
                    "level '{s}': N% (a share of the lower flat-out peak) or MiB/s pinned, {MIN_LEVEL_MIB_S} to {MAX_LEVEL_MIB_S}"
                )),
            },
        }
    }

    /// Levels, comma-separated (`load_levels`, `load-levels=`): 1 to `MAX_LEVELS` of them
    pub fn parse_list(s: &str) -> Result<Vec<Level>, String> {
        let levels = s.split(',').map(Level::parse).collect::<Result<Vec<_>, _>>()?;
        if levels.len() > MAX_LEVELS {
            return Err(format!("{} levels: at most {MAX_LEVELS}, each adding its pairs of windows", levels.len()));
        }
        Ok(levels)
    }

    /// Its MiB/s: a share of `base` to the whole MiB/s, so the pin the report prints gives it back exactly
    pub fn mib_s(self, base: f64) -> f64 {
        match self {
            Level::Percent(p) => (base * p / 100.0).round().max(MIN_LEVEL_MIB_S),
            Level::MiBs(m) => m,
        }
    }

    pub fn is_pinned(self) -> bool {
        matches!(self, Level::MiBs(_))
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Level::Percent(p) => write!(f, "{p}%"),
            Level::MiBs(m) => write!(f, "{m}"),
        }
    }
}

impl Serialize for Level {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Level {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Level::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A step's matched levels as resolved (`resolve`): each as given, what a share is of, and each one's MiB/s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Levels {
    pub given: Vec<Level>,
    /// The lower of the flat-out pairs' median MiB/s under NT writes and under reads (normally the writes'): what a share is of
    pub base_mib_s: f64,
    /// Each level's MiB/s, the loaders' between them
    pub mib_s: Vec<f64>,
}

impl Levels {
    /// The command line's pin of them, `4029,8059`: the same loads in another run, whatever its peaks
    pub fn pin(&self) -> String {
        self.mib_s.iter().map(|m| format!("{m:.0}")).collect::<Vec<_>>().join(",")
    }
}

/// The flat-out windows' median MiB/s, (under NT writes, under reads); 0 for a kind with none
pub fn peaks(windows: &[WindowRecord]) -> (f64, f64) {
    let peak = |kind| median(&windows.iter().filter(|w| w.kind == kind && w.level.is_none()).map(|w| w.mib_s).collect::<Vec<_>>());
    (peak(Kind::Writes), peak(Kind::Reads))
}

/// The levels `given` resolve to from a step's flat-out peaks: a share is of the lower of the two, so either kind can reach it
pub fn resolve(given: &[Level], peak_writes: f64, peak_reads: f64) -> Levels {
    let base_mib_s = peak_writes.min(peak_reads);
    Levels { given: given.to_vec(), base_mib_s, mib_s: given.iter().map(|level| level.mib_s(base_mib_s)).collect() }
}

/// A paced block's start: its deadline `due`, the clock `now` and the pace, `interval` ticks a block. On time it waits for its deadline;
/// late by an interval or less it starts now with its deadline kept, so the next block catches up; later than that it starts now as if
/// due now, never bursting to make up the rest (a shortfall shows in the reached MiB/s), and has fallen behind (`true`).
pub fn pace(due: u64, now: u64, interval: u64) -> (u64, bool) {
    if now > due.saturating_add(interval) { (now, true) } else { (due, false) }
}

/// Loader `index` of `loaders`' first deadline when a paced load starts at `now`: `index / loaders` of an interval after the first's, so
/// their blocks interleave and the load is steady at microsecond scale, where the probe's 1 MiB steps came in bursts
pub fn stagger(now: u64, interval: u64, index: usize, loaders: usize) -> u64 {
    now + interval * index as u64 / loaders.max(1) as u64
}

/// The ticks between a loader's blocks for `loaders` to move `mib_s` between them, the TSC at `ghz`: 1 at least, as 0 is flat out
pub fn interval_ticks(mib_s: f64, loaders: usize, ghz: f64) -> u64 {
    // A loader's share, in bytes a second
    let share = mib_s.max(MIN_LEVEL_MIB_S) * MIB / loaders.max(1) as f64;
    ((BLOCK as f64 * ghz * 1e9 / share).round() as u64).max(1)
}

/// A loaded step's settings, from its config's parameters (`load_levels`, `load_pairs`, `load_window_ms`) or its test's defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub pairs: usize,
    pub window: Duration,
    /// Lat-Turnaround-DRAM's matched levels; Lat-Loaded-DRAM has none
    pub levels: Vec<Level>,
}

impl Params {
    /// Test `name`'s settings in `config`
    pub fn of(name: &str, config: &TestMemoryConfig) -> Self {
        let context = config.parameter_context.as_ref();
        let turnaround = name == TURNAROUND_NAME;
        let pairs = context.and_then(|c| c.load_pairs).unwrap_or(if turnaround { TURNAROUND_PAIRS } else { LOADED_PAIRS });
        let window_ms = context.and_then(|c| c.load_window_ms).unwrap_or(DEFAULT_WINDOW_MS).clamp(1, MAX_WINDOW_MS);
        let mut levels = match context.and_then(|c| c.load_levels.clone()) {
            _ if !turnaround => Vec::new(),
            Some(levels) if !levels.is_empty() => levels,
            _ => DEFAULT_LEVELS.to_vec(),
        };
        levels.truncate(MAX_LEVELS);
        Params { pairs: pairs.clamp(1, MAX_PAIRS) as usize, window: Duration::from_millis(window_ms.into()), levels }
    }

    /// The settle: `SETTLE`, or the window when it is shorter
    pub fn settle(&self) -> Duration {
        SETTLE.min(self.window)
    }

    /// The schedule a step with `loaders` loaders runs: the idle windows alone with none
    pub fn schedule(&self, loaders: usize) -> Schedule {
        if loaders == 0 {
            Schedule::idle(self.window, self.settle())
        } else {
            Schedule::new(self.pairs, self.levels.len(), self.window, self.settle())
        }
    }
}

/// How long test `name`'s step of `config` on these CPUs takes, as its schedule says: the ticker's estimate
pub fn expected(name: &str, config: &TestMemoryConfig, assignments: &[CpuAssignment]) -> Duration {
    let loaders = loaders_of(&assign_roles(assignments, crate::cpu_topology::get_physical_core_for_cpu));
    Duration::from_secs_f64(Params::of(name, config).schedule(loaders).expected_secs())
}

/// Where the measurer ran
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Placement {
    pub thread: usize,
    pub cpu: usize,
    pub core: usize,
    pub node: u32,
}

/// One window as measured: its mean latency over its timed reads, and the loaders' MiB/s over exactly that span.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowRecord {
    pub index: usize,
    pub kind: Kind,
    pub level: Option<usize>,
    pub pair: Option<usize>,
    /// What the loaders were held to, under a level
    pub target_mib_s: Option<f64>,
    pub mean_ns: f64,
    pub mib_s: f64,
    pub reads: u64,
    /// The loaders' blocks that came more than an interval late, under a level
    pub fell_behind: u64,
}

/// The measurer's part of a loaded step, on its `LatencyTestStats`: the roles, the schedule, every window it finished, and a histogram of
/// its samples per row and kind (`Window::cell`). `partial` when a stop cut the schedule short.
#[derive(Debug, Clone)]
pub struct LoadedPart {
    pub roles: Vec<Role>,
    pub measurer: Placement,
    pub schedule: Schedule,
    pub windows: Vec<WindowRecord>,
    pub histograms: Vec<Histogram>,
    /// The levels as the flat-out pairs resolved them; `None` with none, or when a stop came first
    pub levels: Option<Levels>,
    pub chain_bytes: usize,
    pub width: crate::simd::Width,
    pub partial: bool,
}

impl LoadedPart {
    /// Thread `thread`'s role, as the step's (`Step::role`): past the step's threads, sitting out
    pub fn role(&self, thread: usize) -> Role {
        self.roles.get(thread).copied().unwrap_or(Role::SitsOut)
    }
}

/// A row's figures under one kind of load: the median of its windows' means with [lowest-highest], the P50 and P99 of its samples, and
/// the loaders' median MiB/s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub ns: f64,
    pub lowest: f64,
    pub highest: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ns: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_ns: Option<f64>,
    pub mib_s: f64,
    pub windows: usize,
}

/// A level's turnaround: the median of its pairs' differences (under writes less under reads), signed, with [lowest-highest].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turnaround {
    pub ns: f64,
    pub lowest: f64,
    pub highest: f64,
    pub pairs: Vec<f64>,
}

/// A row's load
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Load {
    Idle,
    Level(usize),
    Full,
}

/// One row of the table: idle, a level, or flat out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub load: Load,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_mib_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<Cell>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes: Option<Cell>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<Cell>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turnaround: Option<Turnaround>,
    /// A level's: whether both kinds reached within `MATCH` of its target
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<bool>,
}

/// A loaded step as reported: who did what, the geometry, the rows, and every raw window they came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadedSummary {
    pub measurer: Placement,
    pub loaders: Vec<usize>,
    pub sat_out: Vec<usize>,
    pub chain_bytes: usize,
    /// The L3 the chain is sized by (one instance, CPUID's; `extent_bytes`), which the runner sets: under 16 of them some of its reads may
    /// hit the L3, and the report warns. 0 when unknown
    #[serde(default, skip_serializing_if = "is_zero")]
    pub l3_bytes: usize,
    pub cell_bytes: usize,
    /// The loaders' kernels' width, in bits
    pub width: u32,
    pub block_bytes: usize,
    pub window_ms: f64,
    pub settle_ms: f64,
    /// Lat-Turnaround-DRAM's levels, as given and resolved: absent for Lat-Loaded-DRAM, or a step a stop cut short before them
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub levels: Option<Levels>,
    pub rows: Vec<Row>,
    pub windows: Vec<WindowRecord>,
    pub partial: bool,
}

impl LoadedSummary {
    /// The step's one line: under a level, the turnaround at the highest level both kinds matched; else the flat-out reads' latency
    /// under each load
    pub fn headline(&self) -> String {
        let levels: Vec<&Row> = self.rows.iter().filter(|r| matches!(r.load, Load::Level(_))).collect();
        if !levels.is_empty() {
            let best = levels
                .iter()
                .filter(|r| r.matched == Some(true))
                .filter_map(|r| Some((r.target_mib_s?, r.turnaround.as_ref()?.ns)))
                .max_by(|a, b| a.0.total_cmp(&b.0));
            return match best {
                Some((target, ns)) => format!("Turnaround {ns:+.1} ns at {target:.0} MiB/s"),
                None => "Turnaround not matched at any level".to_string(),
            };
        }
        let at = |cell: &Option<Cell>| cell.as_ref().map_or("-".to_string(), |c| format!("{:.1} ns @ {:.0} MiB/s", c.ns, c.mib_s));
        let row = |load: Load| self.rows.iter().find(|r| r.load == load);
        match (row(Load::Full), row(Load::Idle).and_then(|r| r.idle.as_ref())) {
            (Some(full), _) => format!("Under load: writes {}, reads {}", at(&full.writes), at(&full.reads)),
            (None, Some(idle)) => format!("Under load: no loaders, idle {:.1} ns", idle.ns),
            (None, None) => "Under load: no window measured".to_string(),
        }
    }
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The median of `values` (the mean of the middle two of an even count); 0 with none
fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    match v.len() {
        0 => 0.0,
        n if n % 2 == 1 => v[n / 2],
        n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
    }
}

/// The lowest and highest of `values`
fn range(values: &[f64]) -> (f64, f64) {
    values.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)))
}

/// The table's rows from the raw windows: idle, each of `levels` levels it has windows of, then flat out. `tails` gives a cell's (P50,
/// P99) by its histogram's index (`Window::cell`), when it has samples. Pure, so the statistic can be changed and rerun on windows already
/// taken.
pub fn rows(windows: &[WindowRecord], levels: usize, tails: &dyn Fn(usize) -> Option<(f64, f64)>) -> Vec<Row> {
    let cell = |kind: Kind, level: Option<usize>| -> Option<Cell> {
        let of: Vec<&WindowRecord> = windows.iter().filter(|w| w.kind == kind && w.level == level).collect();
        if of.is_empty() {
            return None;
        }
        let ns: Vec<f64> = of.iter().map(|w| w.mean_ns).collect();
        let mib: Vec<f64> = of.iter().map(|w| w.mib_s).collect();
        let (lowest, highest) = range(&ns);
        let tail = tails(Window { kind, level, pair: None }.cell());
        Some(Cell {
            ns: median(&ns),
            lowest,
            highest,
            p50_ns: tail.map(|t| t.0),
            p99_ns: tail.map(|t| t.1),
            mib_s: median(&mib),
            windows: of.len(),
        })
    };
    let row = |load| Row { load, target_mib_s: None, idle: None, writes: None, reads: None, turnaround: None, matched: None };
    let mut rows = Vec::new();
    if let Some(idle) = cell(Kind::Park, None) {
        rows.push(Row { idle: Some(idle), ..row(Load::Idle) });
    }
    for level in 0..levels {
        let (writes, reads) = (cell(Kind::Writes, Some(level)), cell(Kind::Reads, Some(level)));
        // A level a stop came before has no row
        if writes.is_none() && reads.is_none() {
            continue;
        }
        let target_mib_s = windows.iter().filter(|w| w.level == Some(level)).find_map(|w| w.target_mib_s);
        let near = |c: &Option<Cell>, target: f64| c.as_ref().is_some_and(|c| (c.mib_s / target - 1.0).abs() <= MATCH);
        let matched = target_mib_s.map(|t| near(&writes, t) && near(&reads, t));
        // Each pair's difference, under writes less under reads, in the pairs' order
        let mut pairs: Vec<(usize, f64)> = Vec::new();
        for w in windows.iter().filter(|w| w.level == Some(level) && w.kind == Kind::Writes) {
            if let Some(r) = windows.iter().find(|r| r.level == Some(level) && r.kind == Kind::Reads && r.pair == w.pair) {
                pairs.push((w.pair.unwrap_or(0), w.mean_ns - r.mean_ns));
            }
        }
        pairs.sort_by_key(|&(p, _)| p);
        let pairs: Vec<f64> = pairs.into_iter().map(|(_, d)| d).collect();
        let turnaround = (!pairs.is_empty()).then(|| {
            let (lowest, highest) = range(&pairs);
            Turnaround { ns: median(&pairs), lowest, highest, pairs }
        });
        rows.push(Row { target_mib_s, writes, reads, turnaround, matched, ..row(Load::Level(level)) });
    }
    let (writes, reads) = (cell(Kind::Writes, None), cell(Kind::Reads, None));
    if writes.is_some() || reads.is_some() {
        rows.push(Row { writes, reads, ..row(Load::Full) });
    }
    rows
}

/// A measurer's part as reported: the threads by role, the geometry, and the rows from its windows (`rows`).
pub fn summarize(part: &LoadedPart) -> LoadedSummary {
    let threads = |want: fn(Role) -> bool| part.roles.iter().enumerate().filter(|&(_, &r)| want(r)).map(|(t, _)| t).collect();
    let tails = |cell: usize| part.histograms.get(cell).filter(|h| h.samples() > 0).map(|h| (h.percentile(50.0), h.percentile(99.0)));
    LoadedSummary {
        measurer: part.measurer,
        loaders: threads(|r| matches!(r, Role::Loads { .. })),
        sat_out: threads(|r| r == Role::SitsOut),
        chain_bytes: part.chain_bytes,
        l3_bytes: 0,
        cell_bytes: CELL,
        width: part.width.bits(),
        block_bytes: BLOCK,
        window_ms: part.schedule.window.as_secs_f64() * 1e3,
        settle_ms: part.schedule.settle.as_secs_f64() * 1e3,
        levels: part.levels.clone(),
        rows: rows(&part.windows, part.schedule.levels, &tails),
        windows: part.windows.clone(),
        partial: part.partial,
    }
}

/// A random pointer chase through `n` cells `stride` u64s apart from `base`, as `latency_tests::build_cycle_u64` lays one through
/// slots: each cell's first word holds the next cell's address, one cycle through all `n` (Sattolo's, in insertion form), and no other
/// word is touched.
///
/// # Safety
/// `base` is valid for `n * stride` u64s and aligned to one.
unsafe fn build_cycle_strided(base: *mut u64, n: usize, stride: usize, seed: u64) {
    if n == 0 {
        return;
    }
    let mut rng = 0x123456789ABCDEFu64.wrapping_add(seed);
    let cell = move |i: usize| base.add(i * stride);
    *base = base as u64;
    for i in 1..n {
        let earlier = cell(crate::latency_tests::below(&mut rng, i));
        *cell(i) = *earlier;
        *earlier = cell(i) as u64;
    }
}

/// The measurer's words for the loaders, on a 128 B line pair of their own, so the adjacent-line prefetcher doesn't pull them in with
/// another's: the phase (the window's number and its load) and, stored before it, the window's pace (ticks a block, 0 flat out)
#[repr(align(128))]
struct Phase {
    word: AtomicU64,
    interval: AtomicU64,
}

/// A loader's running counts, on a line pair of its own: its bytes, and its blocks that fell behind their pace
#[repr(align(128))]
struct Counts {
    bytes: AtomicU64,
    behind: AtomicU64,
}

/// One loaded step, shared by its threads: their roles, the barrier the participants (the measurer and the loaders) start at, the phase
/// word and pace the measurer drives the schedule with, and each loader's running counts.
pub struct Step {
    roles: Box<[Role]>,
    loaders: usize,
    measurer: Placement,
    barrier: Barrier,
    phase: Phase,
    counts: Box<[Counts]>,
    params: Params,
}

impl Step {
    /// Test `name`'s step on the threads of `assignments` (`ThreadPool::cpu_assignments`), with `config`'s settings
    pub fn new(name: &str, assignments: &[CpuAssignment], config: &TestMemoryConfig) -> Self {
        Step::with_cores(assignments, crate::cpu_topology::get_physical_core_for_cpu, name, config)
    }

    /// `new`, a CPU's physical core from `core_of`
    fn with_cores(assignments: &[CpuAssignment], core_of: impl Fn(usize) -> usize, name: &str, config: &TestMemoryConfig) -> Self {
        let roles = assign_roles(assignments, &core_of);
        let loaders = loaders_of(&roles);
        let measurer = assignments.iter().find(|&&(t, _, _)| t == 0).map_or(Placement::default(), |&(thread, cpu, node)| Placement {
            thread,
            cpu,
            core: core_of(cpu),
            node,
        });
        Step {
            roles: roles.into(),
            loaders,
            measurer,
            barrier: Barrier::new(1 + loaders),
            phase: Phase { word: AtomicU64::new(phase_word(0, Kind::Park)), interval: AtomicU64::new(0) },
            counts: (0..loaders).map(|_| Counts { bytes: AtomicU64::new(0), behind: AtomicU64::new(0) }).collect(),
            params: Params::of(name, config),
        }
    }

    /// Thread `thread`'s role: past the step's threads, sitting out
    fn role(&self, thread: usize) -> Role {
        self.roles.get(thread).copied().unwrap_or(Role::SitsOut)
    }

    /// Window `window`'s load at `interval` ticks a loader's block (0 flat out), for the loaders: the pace first, the phase word after it
    /// (Release), so a loader that sees the phase sees its pace
    fn set_phase(&self, window: u64, kind: Kind, interval: u64) {
        self.phase.interval.store(interval, Ordering::Relaxed);
        self.phase.word.store(phase_word(window, kind), Ordering::Release);
    }

    /// Every loader's (bytes, blocks fallen behind) so far
    fn loaded(&self) -> (u64, u64) {
        self.counts
            .iter()
            .fold((0, 0), |(bytes, behind), c| (bytes + c.bytes.load(Ordering::Relaxed), behind + c.behind.load(Ordering::Relaxed)))
    }
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Step {{ threads: {}, loaders: {}, measurer: {:?} }}", self.roles.len(), self.loaders, self.measurer)
    }
}

/// Ends the schedule however the measurer leaves it, a panic too: the loaders see Done within a block and finish
struct Finish<'s>(&'s Step);

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.0.set_phase(0, Kind::Done, 0);
    }
}

/// A test function's arguments, for the role it runs
struct Call<'a> {
    name: &'static str,
    blocks: &'a [AllocationBlock],
    thread_id: usize,
    error_mode: ErrorMode,
    timing: &'a TestTiming,
    config: &'a TestMemoryConfig,
    progress: Option<&'a TestProgress>,
}

impl<'a> Call<'a> {
    fn runner(&self, action: TestAction) -> (TestRunner<'a>, TestBlock<'a>) {
        TestRunner::new(self.blocks, self.thread_id, self.error_mode, self.timing, self.config, self.progress, self.name, action)
    }
}

/// Lat-Loaded-DRAM (TODO 117): a read's latency idle and under the other cores' flat-out NT writes and reads. In a run every worker of
/// the step runs it, sharing `config.loaded` (`ThreadPool::execute_test`), each in its role; called without one (a unit test), the
/// thread measures alone, the idle windows only.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_loaded_dram_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> LatencyTestStats {
    // SAFETY: the caller's
    unsafe { run(Call { name: LOADED_NAME, blocks, thread_id, error_mode, timing, config, progress }) }
}

/// Lat-Turnaround-DRAM (TODO 117): Lat-Loaded-DRAM's rows, then a read's latency under NT writes and under reads at matched levels (25,
/// 50 and 75% of the lower flat-out peak by default), each level's turnaround the median of its ABBA pairs' differences. Shared and
/// called as Lat-Loaded-DRAM is.
#[doc = include_str!("test_fn_safety.md")]
pub unsafe fn lat_turnaround_dram_multi(
    blocks: &[AllocationBlock], thread_id: usize, error_mode: ErrorMode, timing: &TestTiming, config: &TestMemoryConfig,
    progress: Option<&TestProgress>,
) -> LatencyTestStats {
    // SAFETY: the caller's
    unsafe { run(Call { name: TURNAROUND_NAME, blocks, thread_id, error_mode, timing, config, progress }) }
}

/// One thread's part in a loaded step: its role's, chosen once.
///
/// # Safety
/// As the test functions'.
unsafe fn run(call: Call) -> LatencyTestStats {
    let alone;
    let (step, me) = match call.config.loaded.as_deref() {
        Some(step) => (step, call.thread_id),
        None => {
            alone = Step::with_cores(&[], |cpu| cpu, call.name, call.config);
            (&alone, 0)
        }
    };
    let role = step.role(me);
    log::debug!("[Thread {}] {}: {} ({} loaders)", call.thread_id, call.name, role.name(), step.loaders);
    match role {
        Role::Measures => measure(step, &call),
        Role::Loads { index } => load(step, index, &call),
        Role::SitsOut => sit_out(&call),
    }
}

/// The measurer: builds its chain, waits for the loaders, then runs the schedule, timing each window's reads.
///
/// # Safety
/// As the test functions'.
unsafe fn measure(step: &Step, call: &Call) -> LatencyTestStats {
    use std::arch::x86_64::{__rdtscp, _rdtsc};
    let ghz = call.config.tsc_frequency_ghz;
    assert!(ghz > 0.0, "{}: no TSC frequency: the latency tests time by the TSC", call.name);
    let (mut runner, extent) = call.runner(TestAction::Latency);
    let schedule = step.params.schedule(step.loaders);
    let cells = extent.test_size / CELL;
    let base = extent.ptr as *mut u64;
    build_cycle_strided(base, cells, CELL / 8, call.thread_id as u64);
    // Before the clock: about 229 KB each
    let mut histograms: Vec<Histogram> = (0..schedule.cells()).map(|_| Histogram::new()).collect();
    runner.setup_done(crate::ops::Op::ChainBuild, 0);
    // The loaders' fills done too: the schedule from here
    step.barrier.wait();
    runner.restart_clock();
    let finish = Finish(step);
    runner.begin_cycle();

    let ticks = |d: Duration| (d.as_nanos() as f64 * ghz) as u64;
    let (window_ticks, settle_ticks) = (ticks(schedule.window), ticks(schedule.settle));
    let mut aux = 0u32;
    let mut ptr = base;
    let mut records: Vec<WindowRecord> = Vec::with_capacity(schedule.windows.len());
    let mut levels: Option<Levels> = None;
    let mut partial = false;
    for (index, &window) in schedule.windows.iter().enumerate().take_while(|_| cells > 0) {
        // A level's MiB/s from this step's flat-out pairs, which come before every level's
        let target_mib_s = window.level.map(|level| {
            let levels = levels.get_or_insert_with(|| {
                let (writes, reads) = peaks(&records);
                let levels = resolve(&step.params.levels, writes, reads);
                log::debug!(
                    "[Thread {}] {}: levels {:?} of {:.0} MiB/s (flat out: NT writes {writes:.0}, reads {reads:.0}): {:?} MiB/s",
                    call.thread_id,
                    call.name,
                    levels.given,
                    levels.base_mib_s,
                    levels.mib_s
                );
                levels
            });
            levels.mib_s[level]
        });
        let interval = target_mib_s.map_or(0, |mib_s| interval_ticks(mib_s, step.loaders, ghz));
        step.set_phase(index as u64 + 1, window.kind, interval);
        // The settle: the loaders onto their new load, untimed
        let settle_end = _rdtsc() + settle_ticks;
        loop {
            let (next, _, end) = crate::latency_tests::chase(ptr, READS);
            ptr = next;
            if end >= settle_end || runner.shutdown_requested() {
                break;
            }
        }
        let (before, behind_before) = step.loaded();
        let from = __rdtscp(&mut aux);
        let window_end = from + window_ticks;
        let histogram = &mut histograms[window.cell()];
        let (mut timed, mut samples) = (0u64, 0u64);
        loop {
            let (next, start, end) = crate::latency_tests::chase(ptr, READS);
            ptr = next;
            timed += end - start;
            samples += 1;
            histogram.add((end - start) as f64 / READS as f64 / ghz);
            if end >= window_end {
                break;
            }
            if runner.shutdown_requested() {
                partial = true;
                break;
            }
        }
        let (after, behind_after) = step.loaded();
        let to = __rdtscp(&mut aux);
        // A window cut short isn't kept
        if partial {
            break;
        }
        let secs = (to - from) as f64 / (ghz * 1e9);
        let record = WindowRecord {
            index,
            kind: window.kind,
            level: window.level,
            pair: window.pair,
            target_mib_s,
            mean_ns: timed as f64 / (samples as usize * READS) as f64 / ghz,
            mib_s: (after - before) as f64 / MIB / secs,
            reads: samples * READS as u64,
            fell_behind: behind_after - behind_before,
        };
        log::debug!(
            "[Thread {}] {} window {index} ({:?}, held to {:?} MiB/s): {:.1} ns over {} reads, the loaders at {:.0} MiB/s, {} behind",
            call.thread_id,
            call.name,
            record.kind,
            record.target_mib_s,
            record.mean_ns,
            record.reads,
            record.mib_s,
            record.fell_behind
        );
        records.push(record);
        runner.update_progress_in_cycle();
    }
    drop(finish);
    std::hint::black_box(ptr);

    let reads: u64 = records.iter().map(|w| w.reads).sum();
    let basic_stats = if partial { runner.finish_aborted(0, reads) } else { runner.finish_completed(reads) };
    let part = LoadedPart {
        roles: step.roles.to_vec(),
        measurer: step.measurer,
        schedule,
        windows: records,
        histograms,
        levels,
        chain_bytes: cells * CELL,
        width: crate::simd::test_width(),
        partial,
    };
    LatencyTestStats { basic_stats, loaded: Some(Box::new(part)), ..LatencyTestStats::from_histogram(Histogram::new()) }
}

/// A loader's wait in an idle window: PAUSE until the phase word moves on, in C0
#[inline(never)]
fn park(phase: &AtomicU64, word: u64) {
    while phase.load(Ordering::Relaxed) == word {
        std::hint::spin_loop();
    }
}

/// The TSC now
#[inline(always)]
fn tsc() -> u64 {
    // SAFETY: every x86-64 CPU has RDTSC, which reads no memory
    unsafe { std::arch::x86_64::_rdtsc() }
}

/// A paced loader's wait for its block's start: PAUSE until the TSC reaches `start`, in C0, or until the phase word moves on (`false`)
#[inline(always)]
fn wait_until(start: u64, phase: &AtomicU64, word: u64) -> bool {
    loop {
        std::hint::spin_loop();
        if phase.load(Ordering::Relaxed) != word {
            return false;
        }
        if tsc() >= start {
            return true;
        }
    }
}

/// A loader: fills its extent, waits for the others, then loads each window as the phase word says, from a random block each time,
/// flat out or held to the window's pace, until Done.
///
/// # Safety
/// As the test functions'.
unsafe fn load(step: &Step, index: usize, call: &Call) -> LatencyTestStats {
    let (mut runner, extent) = call.runner(TestAction::ReadWrite);
    // Once, at the run's width
    let (write, read) = crate::bandwidth_tests::load_kernels(crate::simd::test_width());
    let blocks = extent.test_size / BLOCK;
    let base = extent.ptr;
    // Every page faulted in, and data for the reads to read
    runner.set_stage(Stage::FillingMemory);
    for b in 0..blocks {
        write(base.add(b * BLOCK), BLOCK);
    }
    runner.setup_done(crate::ops::Op::PatternFill, extent.test_size as u64);
    step.barrier.wait();
    runner.restart_clock();
    runner.begin_cycle();

    let counts = &step.counts[index];
    let word = &step.phase.word;
    let mut rng = crate::pattern::fmix(call.config.epoch.key() ^ crate::pattern::fmix(index as u64 + 1)) | 1;
    // Bytes moved, and of them those counted to the runner; blocks fallen behind their pace
    let (mut moved, mut counted, mut acc, mut behind) = (0u64, 0u64, 0u64, 0u64);
    let mut stopped = false;
    while blocks > 0 && !stopped {
        let phase = word.load(Ordering::Acquire);
        let kernel = match kind_of(phase) {
            Kind::Done => break,
            Kind::Park => {
                park(word, phase);
                continue;
            }
            Kind::Writes => write,
            Kind::Reads => read,
        };
        // Its pace, stored before the phase: ticks a block, 0 flat out (no clock read, no wait)
        let interval = step.phase.interval.load(Ordering::Relaxed);
        let mut due = if interval > 0 { stagger(tsc(), interval, index, step.loaders) } else { 0 };
        // A new instance of the load: from a random block
        let mut at = crate::latency_tests::below(&mut rng, blocks);
        loop {
            let mut moved_on = false;
            for _ in 0..BLOCKS_A_CHECK {
                if interval > 0 {
                    let now = tsc();
                    let (start, late) = pace(due, now, interval);
                    if late {
                        behind += 1;
                        counts.behind.store(behind, Ordering::Relaxed);
                    }
                    if now < start && !wait_until(start, word, phase) {
                        moved_on = true;
                        break;
                    }
                    due = start + interval;
                }
                acc ^= kernel(base.add(at * BLOCK), BLOCK);
                moved += BLOCK as u64;
                counts.bytes.store(moved, Ordering::Relaxed);
                at += 1;
                if at == blocks {
                    at = 0;
                }
                if word.load(Ordering::Relaxed) != phase {
                    moved_on = true;
                    break;
                }
            }
            runner.add_bytes((moved - counted) as usize);
            counted = moved;
            runner.update_progress_in_cycle();
            if runner.shutdown_requested() {
                stopped = true;
                break;
            }
            if moved_on {
                break;
            }
        }
    }
    std::hint::black_box(acc);
    let operations = moved / 8;
    let basic_stats =
        if stopped || runner.shutdown_requested() { runner.finish_aborted(0, operations) } else { runner.finish_completed(operations) };
    LatencyTestStats { basic_stats, ..LatencyTestStats::from_histogram(Histogram::new()) }
}

/// A thread that sits out: its one cycle done at once, nothing moved, and back to the worker's wait, its core idle
fn sit_out(call: &Call) -> LatencyTestStats {
    let (mut runner, _) = call.runner(TestAction::Latency);
    runner.restart_clock();
    runner.begin_cycle();
    LatencyTestStats { basic_stats: runner.finish_completed(0), ..LatencyTestStats::from_histogram(Histogram::new()) }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::TestParameterContext;
    use crate::runner::test_support::{KIB, Region};
    use crate::tests::{ChunkMode, ExtentMode};

    /// Thread `t` on CPU `cpus[t]`, its node `node_of(cpu)`
    fn threads(cpus: &[usize], node_of: impl Fn(usize) -> u32) -> Vec<CpuAssignment> {
        cpus.iter().enumerate().map(|(t, &cpu)| (t, cpu, node_of(cpu))).collect()
    }

    /// The threads that load, in their slots' order
    fn loading(roles: &[Role]) -> Vec<usize> {
        let mut by_slot: Vec<(usize, usize)> =
            roles.iter().enumerate().filter_map(|(t, r)| if let Role::Loads { index } = r { Some((*index, t)) } else { None }).collect();
        by_slot.sort();
        assert!(by_slot.iter().enumerate().all(|(i, &(index, _))| i == index), "slots 0..n: {by_slot:?}");
        by_slot.into_iter().map(|(_, t)| t).collect()
    }

    /// Thread 0 measures; one loader a physical core on its node, the first thread of each other core; the rest sit out: the 5700X's
    /// 8 cores of 2 threads under cputype=threads and =cores (the same 7 loaders on the same cores), thread 0 not on CPU 0, a 2-node box,
    /// one thread, and cpus=50% (half the CPUs, both of a core's)
    #[test]
    fn the_roles_are_one_measurer_and_one_loader_a_core_on_its_node() {
        let smt = |cpu: usize| cpu / 2;
        // cputype=threads: CPUs 0-15, a core's two side by side
        let all: Vec<usize> = (0..16).collect();
        let roles = assign_roles(&threads(&all, |_| 0), smt);
        assert_eq!(roles[0], Role::Measures);
        assert_eq!(roles[1], Role::SitsOut, "the measurer's sibling");
        assert_eq!(loading(&roles), [2, 4, 6, 8, 10, 12, 14]);
        assert!((3..16).step_by(2).all(|t| roles[t] == Role::SitsOut), "the loaders' siblings: {roles:?}");
        // cputype=cores: a core's first CPU each, the same cores loading
        let cores: Vec<usize> = (0..8).map(|c| 2 * c).collect();
        let roles = assign_roles(&threads(&cores, |_| 0), smt);
        assert_eq!(roles[0], Role::Measures);
        assert_eq!(loading(&roles), [1, 2, 3, 4, 5, 6, 7]);
        // Ranked: thread 0 on core 2 (CPU 4), its sibling third
        let ranked = [4, 0, 5, 2, 6, 1, 3, 7];
        let roles = assign_roles(&threads(&ranked, |_| 0), smt);
        assert_eq!((roles[0], roles[2]), (Role::Measures, Role::SitsOut));
        assert_eq!(loading(&roles), [1, 3, 4], "cores 0, 1 and 3, each its first thread");
        // Two nodes of 4 cores: node 1's threads sit out
        let roles = assign_roles(&threads(&all, |cpu| u32::from(cpu >= 8)), smt);
        assert_eq!(loading(&roles), [2, 4, 6]);
        assert!((8..16).all(|t| roles[t] == Role::SitsOut));
        // Thread 0 on node 1: node 1 loads
        let roles = assign_roles(&threads(&[8, 9, 10, 0, 12, 2], |cpu| u32::from(cpu >= 8)), smt);
        assert_eq!(loading(&roles), [2, 4]);
        // One thread, and none
        assert_eq!(assign_roles(&threads(&[6], |_| 0), smt), [Role::Measures]);
        assert_eq!(assign_roles(&[], smt), [Role::Measures]);
        // cpus=50%: CPUs 0-7, cores 0-3 both threads
        let roles = assign_roles(&threads(&all[..8], |_| 0), smt);
        assert_eq!(loading(&roles), [2, 4, 6]);
        // No two loaders share a core, nor one the measurer's
        for set in [&all[..], &ranked[..], &cores[..]] {
            let roles = assign_roles(&threads(set, |_| 0), smt);
            let mut used = vec![smt(set[0])];
            for t in loading(&roles) {
                assert!(!used.contains(&smt(set[t])), "{set:?}: thread {t}");
                used.push(smt(set[t]));
            }
        }
    }

    /// Lat-Loaded-DRAM's schedule is idle, 3 ABBA pairs flat out (W R R W W R) and idle, 8 windows of settle and window; the
    /// turnaround's (5 pairs, 2 levels) adds each level's pairs after them, 32 windows, about 38 s; with no loaders, the idle windows
    #[test]
    fn a_schedule_is_abba_pairs_between_idle_windows() {
        let (second, settle) = (Duration::from_secs(1), SETTLE);
        let loaded = Schedule::new(3, 0, second, settle);
        let kinds: Vec<Kind> = loaded.windows.iter().map(|w| w.kind).collect();
        use Kind::{Park, Reads, Writes};
        assert_eq!(kinds, [Park, Writes, Reads, Reads, Writes, Writes, Reads, Park]);
        let pairs: Vec<Option<usize>> = loaded.windows.iter().map(|w| w.pair).collect();
        assert_eq!(pairs, [None, Some(0), Some(0), Some(1), Some(1), Some(2), Some(2), None]);
        assert!(loaded.windows.iter().all(|w| w.level.is_none()));
        assert!((loaded.expected_secs() - 9.6).abs() < 1e-9);
        assert_eq!(loaded.cells(), 3);

        let turnaround = Schedule::new(5, 2, second, settle);
        assert_eq!(turnaround.windows.len(), 32);
        assert!((turnaround.expected_secs() - 38.4).abs() < 1e-9);
        for (level, from) in [(None, 1), (Some(0), 11), (Some(1), 21)] {
            let group = &turnaround.windows[from..from + 10];
            assert!(group.iter().all(|w| w.level == level), "{level:?}");
            for (p, pair) in group.chunks(2).enumerate() {
                let want = if p % 2 == 0 { [Writes, Reads] } else { [Reads, Writes] };
                assert_eq!([pair[0].kind, pair[1].kind], want, "{level:?}, pair {p}");
                assert!(pair.iter().all(|w| w.pair == Some(p)));
            }
        }
        let cells: std::collections::BTreeSet<usize> = turnaround.windows.iter().map(|w| w.cell()).collect();
        assert_eq!(cells, (0..turnaround.cells()).collect::<std::collections::BTreeSet<_>>(), "a histogram a row and kind");

        let idle = Params { pairs: 3, window: second, levels: DEFAULT_LEVELS.to_vec() }.schedule(0);
        assert_eq!(idle.windows, [Window::IDLE; 2]);
        // The settle is never longer than the window
        assert_eq!(Params { pairs: 3, window: Duration::from_millis(5), levels: Vec::new() }.settle(), Duration::from_millis(5));
    }

    /// Each test's settings: Lat-Loaded-DRAM 3 pairs flat out and no levels, whatever the config says of them; Lat-Turnaround-DRAM 5
    /// pairs at 25, 50 and 75%, about 50 s, or the config's; either's pairs and window the config's, within their ranges
    #[test]
    fn each_test_takes_its_defaults_or_the_configs() {
        let mut config = TestMemoryConfig::new(ExtentMode::Absolute { size_bytes: 256 * KIB }, ChunkMode::Whole);
        let loaded = Params::of(LOADED_NAME, &config);
        assert_eq!((loaded.pairs, loaded.window, loaded.levels.as_slice()), (3, Duration::from_secs(1), [].as_slice()));
        let turnaround = Params::of(TURNAROUND_NAME, &config);
        assert_eq!((turnaround.pairs, turnaround.levels.as_slice()), (5, DEFAULT_LEVELS.as_slice()));
        let schedule = turnaround.schedule(7);
        assert_eq!((schedule.windows.len(), schedule.levels), (42, 3));
        assert!((schedule.expected_secs() - 50.4).abs() < 1e-9);

        let pinned = vec![Level::MiBs(4029.0), Level::Percent(60.0)];
        config.parameter_context = Some(TestParameterContext {
            load_levels: Some(pinned.clone()),
            load_pairs: Some(2),
            load_window_ms: Some(250),
            ..Default::default()
        });
        let turnaround = Params::of(TURNAROUND_NAME, &config);
        assert_eq!((turnaround.pairs, turnaround.window, turnaround.levels), (2, Duration::from_millis(250), pinned));
        assert_eq!(Params::of(LOADED_NAME, &config).levels, [], "Lat-Loaded-DRAM has no levels");
        config.parameter_context = Some(TestParameterContext { load_pairs: Some(500), load_window_ms: Some(0), ..Default::default() });
        let clamped = Params::of(LOADED_NAME, &config);
        assert_eq!((clamped.pairs, clamped.window), (50, Duration::from_millis(1)));
    }

    /// A level is a share, more than 0% and at most 100%, or MiB/s pinned, 1 to 1,000,000, written back as given; a list is 1 to 8 of
    /// them, comma-separated
    #[test]
    fn levels_parse_as_shares_or_pinned_mib_s() {
        for (given, level, shown) in [
            ("25%", Level::Percent(25.0), "25%"),
            (" 12.5 % ", Level::Percent(12.5), "12.5%"),
            ("100%", Level::Percent(100.0), "100%"),
            ("8058", Level::MiBs(8058.0), "8058"),
            ("1", Level::MiBs(1.0), "1"),
            ("1000000", Level::MiBs(1e6), "1000000"),
        ] {
            assert_eq!(Level::parse(given), Ok(level), "{given}");
            assert_eq!(level.to_string(), shown);
            assert_eq!(Level::parse(&level.to_string()), Ok(level), "{shown} reads back");
        }
        for bad in ["0%", "-5%", "101%", "%", "25%%", "0", "0.5", "1000001", "fast", "", "inf", "NaN%"] {
            assert!(Level::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Level::parse_list("25%,50%, 8058"), Ok(vec![Level::Percent(25.0), Level::Percent(50.0), Level::MiBs(8058.0)]));
        assert!(Level::parse_list("25%,,50%").is_err());
        assert!(Level::parse_list("1%,2%,3%,4%,5%,6%,7%,8%").is_ok());
        assert!(Level::parse_list("1%,2%,3%,4%,5%,6%,7%,8%,9%").unwrap_err().contains("at most 8"));
        let json = serde_json::to_string(&[Level::Percent(25.0), Level::MiBs(8058.0)]).unwrap();
        assert_eq!(json, r#"["25%","8058"]"#);
        assert_eq!(serde_json::from_str::<Vec<Level>>(&json).unwrap(), [Level::Percent(25.0), Level::MiBs(8058.0)]);
    }

    /// A share is of the lower flat-out peak (the median of each kind's flat-out windows), to the whole MiB/s; a pinned level is its own
    #[test]
    fn levels_resolve_against_the_lower_flat_out_peak() {
        use Kind::{Park, Reads, Writes};
        let full = [
            window(0, Park, None, None, 100.0, 0.0),
            window(1, Writes, None, Some(0), 450.0, 16_000.0),
            window(2, Reads, None, Some(0), 264.0, 27_000.0),
            window(3, Reads, None, Some(1), 266.0, 27_200.0),
            window(4, Writes, None, Some(1), 454.0, 16_234.0),
            // A level's window isn't a peak's
            window(5, Writes, Some(0), Some(0), 113.0, 90_000.0),
        ];
        assert_eq!(peaks(&full), (16_117.0, 27_100.0));
        let levels = resolve(&[Level::Percent(25.0), Level::Percent(50.0), Level::MiBs(9000.0)], 16_117.0, 27_100.0);
        assert_eq!((levels.base_mib_s, levels.mib_s.as_slice()), (16_117.0, [4029.0, 8059.0, 9000.0].as_slice()));
        assert_eq!(levels.pin(), "4029,8059,9000");
        // The reads' peak the lower: a share is of it
        assert_eq!(resolve(&[Level::Percent(50.0)], 30_000.0, 20_000.0).mib_s, [10_000.0]);
        // No flat-out data: 1 MiB/s, never 0
        assert_eq!(resolve(&[Level::Percent(50.0)], 0.0, 0.0).mib_s, [1.0]);
    }

    /// On time a block waits for its deadline; late by an interval or less it starts at once, its deadline kept so the next catches up;
    /// later than that it starts now from now, never bursting, and has fallen behind. Loader i of L starts i/L of an interval in, and a
    /// level's interval is a block over a loader's share, never 0 (flat out)
    #[test]
    fn pacing_waits_staggers_and_caps_its_debt() {
        assert_eq!(pace(1000, 900, 100), (1000, false), "early: waits for its deadline");
        assert_eq!(pace(1000, 1000, 100), (1000, false));
        assert_eq!(pace(1000, 1100, 100), (1000, false), "an interval late: now, the next due at 1100");
        assert_eq!(pace(1000, 1101, 100), (1101, true), "more: from now, fallen behind");
        assert_eq!(pace(u64::MAX - 10, u64::MAX - 5, 100), (u64::MAX - 10, false), "no overflow");
        assert_eq!([0, 1, 2, 3].map(|i| stagger(5000, 400, i, 4)), [5000, 5100, 5200, 5300]);
        assert_eq!(stagger(5000, 400, 0, 0), 5000, "no loaders");
        // 8058 MiB/s over 7 loaders at 3.4 GHz: 1151 MiB/s each, a block every 11,537 ticks (3.4 us)
        assert_eq!(interval_ticks(8058.0, 7, 3.4), 11_537);
        assert_eq!(interval_ticks(1e9, 1, 1.0), 1, "never flat out");
        assert_eq!(interval_ticks(0.0, 1, 1.0), interval_ticks(1.0, 1, 1.0), "at least 1 MiB/s");
    }

    /// The chain at 16 words a cell is one cycle through every cell's first word, and every other word keeps its canary
    #[test]
    fn the_chain_is_one_cycle_through_every_cell_and_touches_nothing_else() {
        const CANARY: u64 = 0x5A5A_C3C3_A5A5_3C3C;
        for n in [1usize, 2, 3, 7, 64, 1000] {
            let mut mem = vec![CANARY; n * 16];
            let base = mem.as_mut_ptr();
            unsafe { build_cycle_strided(base, n, 16, 5) };
            let mut seen = vec![false; n];
            let mut at = base;
            for _ in 0..n {
                let offset = (at as usize - base as usize) / 8;
                assert_eq!(offset % 16, 0, "n={n}: a link into the middle of a cell");
                assert!(!seen[offset / 16], "n={n}: cell {} visited twice", offset / 16);
                seen[offset / 16] = true;
                at = unsafe { *at } as *mut u64;
            }
            assert_eq!(at, base, "n={n}: the chase didn't come back to the start");
            assert!(mem.iter().enumerate().all(|(i, &w)| i % 16 == 0 || w == CANARY), "n={n}: a word outside the cells' first");
        }
    }

    fn window(index: usize, kind: Kind, level: Option<usize>, pair: Option<usize>, ns: f64, mib_s: f64) -> WindowRecord {
        let target_mib_s = level.map(|l| [4000.0, 8000.0][l]);
        WindowRecord { index, kind, level, pair, target_mib_s, mean_ns: ns, mib_s, reads: 1000, fell_behind: 0 }
    }

    fn part(windows: Vec<WindowRecord>, levels: usize) -> LoadedPart {
        let mut histograms: Vec<Histogram> = (0..3 + 2 * levels).map(|_| Histogram::new()).collect();
        for ns in [100.0, 101.0, 102.0, 103.0, 150.0] {
            histograms[0].add(ns);
        }
        LoadedPart {
            roles: vec![Role::Measures, Role::SitsOut, Role::Loads { index: 0 }, Role::Loads { index: 1 }],
            measurer: Placement { thread: 0, cpu: 4, core: 2, node: 0 },
            schedule: Schedule::new(2, levels, Duration::from_secs(1), SETTLE),
            windows,
            histograms,
            levels: None,
            chain_bytes: 1 << 30,
            width: crate::simd::Width::W256,
            partial: false,
        }
    }

    /// A cell is the median of its windows' means with their range and the loaders' median MiB/s; a level's turnaround the median of its
    /// pairs' differences with theirs; a level matches when both kinds reach within 3% of its target; the headline is the turnaround at
    /// the highest level matched, else the flat-out cells, else the idle one
    #[test]
    fn a_summary_takes_medians_ranges_pairs_and_matches() {
        use Kind::{Park, Reads, Writes};
        let idle = |i, ns| window(i, Park, None, None, ns, 0.0);
        let full = vec![
            window(1, Writes, None, Some(0), 450.0, 16000.0),
            window(2, Reads, None, Some(0), 264.0, 27000.0),
            window(3, Reads, None, Some(1), 266.0, 27200.0),
            window(4, Writes, None, Some(1), 454.0, 16200.0),
        ];
        let levels = vec![
            // 4000 MiB/s: both reach it; under writes 113 and 116, under reads 111 and 112
            window(5, Writes, Some(0), Some(0), 113.0, 4000.0),
            window(6, Reads, Some(0), Some(0), 111.0, 3990.0),
            window(7, Reads, Some(0), Some(1), 112.0, 4010.0),
            window(8, Writes, Some(0), Some(1), 116.0, 3995.0),
            // 8000 MiB/s: the reads fall 12% short
            window(9, Writes, Some(1), Some(0), 127.0, 8000.0),
            window(10, Reads, Some(1), Some(0), 121.0, 7000.0),
            window(11, Reads, Some(1), Some(1), 122.0, 7100.0),
            window(12, Writes, Some(1), Some(1), 129.0, 7900.0),
        ];
        let windows: Vec<WindowRecord> =
            std::iter::once(idle(0, 100.0)).chain(full.clone()).chain(levels).chain([idle(13, 104.0)]).collect();
        let summary = summarize(&part(windows, 2));
        assert_eq!((summary.loaders.as_slice(), summary.sat_out.as_slice()), ([2, 3].as_slice(), [1].as_slice()));
        assert_eq!((summary.cell_bytes, summary.block_bytes, summary.width, summary.window_ms), (128, 4096, 256, 1000.0));
        let loads: Vec<Load> = summary.rows.iter().map(|r| r.load).collect();
        assert_eq!(loads, [Load::Idle, Load::Level(0), Load::Level(1), Load::Full]);

        let idle_cell = summary.rows[0].idle.as_ref().unwrap();
        assert_eq!((idle_cell.ns, idle_cell.lowest, idle_cell.highest, idle_cell.windows), (102.0, 100.0, 104.0, 2));
        assert_eq!(idle_cell.p50_ns.map(|p| p.round()), Some(102.0), "its samples' P50");
        let low = &summary.rows[1];
        let (w, r) = (low.writes.as_ref().unwrap(), low.reads.as_ref().unwrap());
        assert_eq!((w.ns, w.lowest, w.highest, w.mib_s), (114.5, 113.0, 116.0, 3997.5));
        assert_eq!((r.ns, r.mib_s, r.p50_ns), (111.5, 4000.0, None));
        let t = low.turnaround.as_ref().unwrap();
        assert_eq!((t.ns, t.lowest, t.highest, t.pairs.as_slice()), (3.0, 2.0, 4.0, [2.0, 4.0].as_slice()));
        assert_eq!((low.target_mib_s, low.matched), (Some(4000.0), Some(true)));
        let high = &summary.rows[2];
        assert_eq!((high.matched, high.turnaround.as_ref().map(|t| t.ns)), (Some(false), Some(6.5)));
        let flat = &summary.rows[3];
        let (w, r) = (flat.writes.as_ref().unwrap(), flat.reads.as_ref().unwrap());
        assert_eq!((w.ns, w.mib_s, r.ns, r.mib_s), (452.0, 16100.0, 265.0, 27100.0));
        assert_eq!((flat.turnaround.as_ref(), flat.matched), (None, None), "flat out isn't matched");
        assert_eq!(summary.headline(), "Turnaround +3.0 ns at 4000 MiB/s");

        // Lat-Loaded-DRAM's: no levels
        let windows: Vec<WindowRecord> = std::iter::once(idle(0, 100.0)).chain(full).chain([idle(5, 104.0)]).collect();
        let summary = summarize(&part(windows, 0));
        assert_eq!(summary.rows.len(), 2);
        assert_eq!(summary.headline(), "Under load: writes 452.0 ns @ 16100 MiB/s, reads 265.0 ns @ 27100 MiB/s");
        // No loaders: the idle windows
        let summary = summarize(&part(vec![idle(0, 100.0), idle(1, 104.0)], 0));
        assert_eq!(summary.headline(), "Under load: no loaders, idle 102.0 ns");
        // A summary goes through the results file and back
        let json = serde_json::to_string(&summary).unwrap();
        assert_eq!(serde_json::from_str::<LoadedSummary>(&json).unwrap(), summary);
    }

    const STEP_CORES: [usize; 4] = [0, 0, 1, 2];

    /// A config of `window_ms` windows over 256 KiB, the TSC taken as 1 GHz
    fn config(window_ms: u32) -> TestMemoryConfig {
        let mut config = TestMemoryConfig::new(ExtentMode::Absolute { size_bytes: 256 * KIB }, ChunkMode::Whole)
            .with_timing(TestTiming::cycles_only(1))
            .with_tsc(1.0);
        config.parameter_context = Some(TestParameterContext { load_window_ms: Some(window_ms), ..Default::default() });
        config
    }

    type TestFn =
        unsafe fn(&[AllocationBlock], usize, ErrorMode, &TestTiming, &TestMemoryConfig, Option<&TestProgress>) -> LatencyTestStats;

    /// Test `name`'s step of `config` on four threads on CPUs 0-3 (thread 1 the measurer's sibling), each on its own region, `during`
    /// run on the test's thread while they work; their results by thread
    fn run_step(
        name: &str, config: TestMemoryConfig, during: impl FnOnce(&[std::thread::ScopedJoinHandle<'_, LatencyTestStats>]),
    ) -> Vec<LatencyTestStats> {
        let test: TestFn = if name == TURNAROUND_NAME { lat_turnaround_dram_multi } else { lat_loaded_dram_multi };
        let assignments = threads(&[0, 1, 2, 3], |_| 0);
        let step = Arc::new(Step::with_cores(&assignments, |cpu| STEP_CORES[cpu], name, &config));
        let config = TestMemoryConfig { loaded: Some(step), ..config };
        let regions: Vec<Region> = (0..4).map(|_| Region::new(&[256 * KIB])).collect();
        let stats = std::thread::scope(|s| {
            let handles: Vec<_> = regions
                .iter()
                .enumerate()
                .map(|(t, region)| {
                    let (blocks, config) = (&region.blocks, &config);
                    // SAFETY: each thread's own region, live for the scope
                    s.spawn(move || unsafe { test(blocks, t, ErrorMode::Log, &config.timing, config, None) })
                })
                .collect();
            during(&handles);
            handles.into_iter().map(|h| h.join().expect("a thread of the step")).collect()
        });
        assert!(regions.iter().all(Region::guards_intact), "a thread wrote outside its region");
        stats
    }

    /// Four threads: thread 0 measures every window of Lat-Loaded-DRAM's schedule, thread 1 (its sibling) sits out, threads 2 and 3
    /// load, and every role finishes with the schedule
    #[test]
    fn a_step_measures_every_row_of_lat_loaded_dram() {
        let stats = run_step(LOADED_NAME, config(20), |_| {});
        let part = stats[0].loaded.as_deref().expect("the measurer's part");
        assert_eq!(part.roles, [Role::Measures, Role::SitsOut, Role::Loads { index: 0 }, Role::Loads { index: 1 }]);
        assert!(!part.partial);
        let kinds: Vec<Kind> = part.windows.iter().map(|w| w.kind).collect();
        assert_eq!(kinds, part.schedule.windows.iter().map(|w| w.kind).collect::<Vec<_>>(), "every window, in order");
        assert_eq!(part.windows.len(), 8);
        assert!(part.windows.iter().all(|w| w.reads > 0 && w.mean_ns > 0.0), "{:?}", part.windows);
        let loaded: f64 = part.windows.iter().filter(|w| w.kind != Kind::Park).map(|w| w.mib_s).sum();
        assert!(loaded > 0.0, "the loaders moved nothing in their windows");
        assert_eq!((part.chain_bytes, part.histograms.len()), (256 * KIB, 3));
        let summary = summarize(part);
        let loads: Vec<Load> = summary.rows.iter().map(|r| r.load).collect();
        assert_eq!(loads, [Load::Idle, Load::Full]);
        let full = &summary.rows[1];
        assert_eq!((full.writes.as_ref().map(|c| c.windows), full.reads.as_ref().map(|c| c.windows)), (Some(3), Some(3)));
        assert_eq!((stats[0].sample_count, stats[0].basic_stats.bytes_processed), (0, 0), "the measurer counts no bytes");
        assert!(stats[0].basic_stats.total_operations > 0, "its reads");
        for (t, s) in stats.iter().enumerate() {
            assert_eq!((s.basic_stats.cycles_completed, s.basic_stats.stopped_by_time_limit), (1, false), "thread {t}");
            assert_eq!(s.loaded.is_some(), t == 0, "thread {t}");
        }
        assert_eq!(stats[1].basic_stats.bytes_processed, 0, "the sibling sits out");
        assert!(stats[2..].iter().all(|s| s.basic_stats.bytes_processed > 0), "each loader loads");
    }

    /// Lat-Turnaround-DRAM's step at one pair a load: its flat-out pairs first, then each default level (25, 50 and 75% of the lower
    /// flat-out peak, to the whole MiB/s) with its windows held to it, the loaders never past it but for the two blocks each its debt
    /// allows, and a row a level between idle and full. (Under it, a loader the scheduler took off its CPU, which only loses blocks; on a
    /// busy machine a whole flat-out window can lose its loaders, and its level's figures with it. The pacing's lower bound is
    /// `a_paced_loader_holds_its_pace_and_counts_what_falls_behind`'s.)
    #[test]
    fn a_step_holds_the_loaders_to_each_level_of_lat_turnaround_dram() {
        let mut config = config(20);
        config.parameter_context.as_mut().unwrap().load_pairs = Some(1);
        let stats = run_step(TURNAROUND_NAME, config, |_| {});
        let part = stats[0].loaded.as_deref().expect("the measurer's part");
        assert!(!part.partial);
        assert_eq!(part.windows.len(), 1 + 2 + 3 * 2 + 1, "idle, a pair flat out, a pair a level, idle");
        let levels = part.levels.as_ref().expect("the levels, resolved");
        let (writes, reads) = peaks(&part.windows);
        assert_eq!(levels, &resolve(&DEFAULT_LEVELS, writes, reads), "from this step's flat-out pairs");
        // Two blocks a loader over a window, in MiB/s (the TSC taken as 1 GHz, as the measurer takes it)
        let slack = (2 * 2 * BLOCK) as f64 / MIB / part.schedule.window.as_secs_f64();
        for w in &part.windows {
            assert_eq!(w.target_mib_s, w.level.map(|l| levels.mib_s[l]), "window {}", w.index);
            if let Some(target) = w.target_mib_s {
                // Never past its pace: its debt is an interval at most
                assert!(w.mib_s <= 1.05 * target + slack, "window {}: {:.1} MiB/s held to {target:.0}", w.index, w.mib_s);
            }
        }
        let held: f64 = part.windows.iter().filter(|w| w.level.is_some()).map(|w| w.mib_s).sum();
        assert!(held > 0.0, "the loaders moved nothing at any level");
        assert_eq!(part.histograms.len(), 9, "idle, flat out and three levels, a kind each");
        let summary = summarize(part);
        let loads: Vec<Load> = summary.rows.iter().map(|r| r.load).collect();
        assert_eq!(loads, [Load::Idle, Load::Level(0), Load::Level(1), Load::Level(2), Load::Full]);
        assert!(summary.rows[1..4].iter().all(|r| r.turnaround.as_ref().is_some_and(|t| t.pairs.len() == 1)), "{:?}", summary.rows);
        assert_eq!(summary.levels.as_ref(), Some(levels));
        assert!(summary.headline().starts_with("Turnaround "), "{}", summary.headline());
        assert!(stats.iter().all(|s| s.basic_stats.cycles_completed == 1));
    }

    /// One loader held to a pace moves a block an interval, within 5% over 100 ms, and never more: its debt is an interval at most; held
    /// to more than it can move, every block counts as fallen behind; and it ends on Done. The test's thread drives the phase word as the
    /// measurer does, the loader at a run's worker priority. The scheduler can still take it off its CPU (for a quantum, near 190 ms on
    /// Windows Server, with nextest running a process a CPU; a hypervisor's steal), which only loses blocks, so every window is held to
    /// the upper bound and the best of up to 20 to the lower.
    #[test]
    fn a_paced_loader_holds_its_pace_and_counts_what_falls_behind() {
        let config = config(5);
        // Thread 0 (the test's thread) and thread 1, the loader, on cores 0 and 1
        let step = Step::with_cores(&threads(&[0, 1], |_| 0), |cpu| cpu, TURNAROUND_NAME, &config);
        let region = Region::new(&[256 * KIB]);
        let call = Call {
            name: TURNAROUND_NAME,
            blocks: &region.blocks,
            thread_id: 1,
            error_mode: ErrorMode::Log,
            timing: &config.timing,
            config: &config,
            progress: None,
        };
        let interval = 40_000u64;
        let (held, (late_blocks, late), stats) = std::thread::scope(|s| {
            // SAFETY: the region's blocks, live for the scope
            let loader = s.spawn(|| {
                // As a run's workers (`priority=high`), so another process's threads don't take its CPU
                crate::runner::ThreadPriority::High.apply().expect("the loader's priority");
                unsafe { load(&step, 0, &call) }
            });
            step.barrier.wait();
            step.set_phase(1, Kind::Reads, interval);
            std::thread::sleep(Duration::from_millis(20));
            // Each window's bytes over those its ticks allow, and its blocks fallen behind
            let mut held: Vec<(f64, u64)> = Vec::new();
            while held.len() < 20 && !held.iter().any(|&(h, _)| h > 0.95) {
                let ((from, behind_from), t0) = (step.loaded(), tsc());
                std::thread::sleep(Duration::from_millis(100));
                let ((to, behind_to), t1) = (step.loaded(), tsc());
                held.push(((to - from) as f64 / ((t1 - t0) / interval * BLOCK as u64) as f64, behind_to - behind_from));
            }
            // A pace no loader can hold
            step.set_phase(2, Kind::Writes, 1);
            std::thread::sleep(Duration::from_millis(5));
            let (bytes_before, behind_before) = step.loaded();
            std::thread::sleep(Duration::from_millis(20));
            let (bytes_after, behind_after) = step.loaded();
            step.set_phase(0, Kind::Done, 0);
            let stats = loader.join().expect("the loader");
            (held, ((bytes_after - bytes_before) / BLOCK as u64, behind_after - behind_before), stats)
        });
        assert!(held.iter().all(|&(h, _)| h < 1.05), "past its pace, a block every {interval} ticks: {held:?}");
        assert!(held.iter().any(|&(h, _)| h > 0.95), "short of its pace, a block every {interval} ticks: {held:?} (held, behind)");
        assert!(late_blocks > 0 && late as f64 >= 0.9 * late_blocks as f64, "{late} of {late_blocks} blocks behind");
        assert_eq!(stats.basic_stats.cycles_completed, 1);
        assert!(region.guards_intact());
    }

    /// A thread that sits out returns at once, waiting for no one: alone here, it would wait forever at the participants' barrier
    #[test]
    fn a_sit_out_returns_at_once() {
        let assignments = threads(&[0, 1, 2, 3], |_| 0);
        let step = Arc::new(Step::with_cores(&assignments, |cpu| STEP_CORES[cpu], LOADED_NAME, &config(5)));
        let config = TestMemoryConfig { loaded: Some(step), ..config(5) };
        let region = Region::new(&[64 * KIB]);
        // SAFETY: the region's blocks, live for the call
        let stats = unsafe { lat_loaded_dram_multi(&region.blocks, 1, ErrorMode::Log, &config.timing, &config, None) };
        assert_eq!((stats.basic_stats.bytes_processed, stats.basic_stats.cycles_completed, stats.sample_count), (0, 1, 0));
        assert!(stats.loaded.is_none());
    }

    /// Called with no step (no loaders), the measurer measures the two idle windows, and says it had no loaders
    #[test]
    fn with_no_step_it_measures_the_idle_rows() {
        let region = Region::new(&[256 * KIB]);
        let config = config(5);
        // SAFETY: the region's blocks, live for the call
        let stats = unsafe { lat_loaded_dram_multi(&region.blocks, 0, ErrorMode::Log, &config.timing, &config, None) };
        let part = stats.loaded.as_deref().expect("the measurer's part");
        assert_eq!(part.windows.iter().map(|w| w.kind).collect::<Vec<_>>(), [Kind::Park, Kind::Park]);
        assert_eq!(part.roles, [Role::Measures]);
        let summary = summarize(part);
        assert_eq!(summary.rows.iter().map(|r| r.load).collect::<Vec<_>>(), [Load::Idle]);
        assert!(summary.headline().starts_with("Under load: no loaders, idle "), "{}", summary.headline());
        assert!(region.guards_intact());
    }

    /// A stop (Ctrl+C, the window's Stop) mid-schedule ends every role: the measurer with the windows it finished, marked partial, and
    /// the loaders with it. The flag is the process's: nextest's one process a test keeps it to this one; it is set again until the
    /// threads end, should a session in the same process (`cargo test`) clear it under them.
    #[test]
    fn a_stop_ends_every_role_with_a_partial_result() {
        let stats = run_step(LOADED_NAME, config(1000), |handles| {
            std::thread::sleep(Duration::from_millis(50));
            while !handles.iter().all(|h| h.is_finished()) {
                crate::runner::request_stop();
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        crate::runner::clear_stop();
        let part = stats[0].loaded.as_deref().expect("the measurer's part");
        assert!(part.partial);
        assert!(part.windows.len() < part.schedule.windows.len(), "{} windows", part.windows.len());
        assert!(stats.iter().all(|s| s.basic_stats.cycles_completed == 1));
    }
}
