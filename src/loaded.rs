//! The loaded latency tests (TODO 117): a read's latency while the other cores load DRAM, which no unloaded test sees. On daddy-petz
//! (`doc/todo/117-write-latency.md`) tWR 12 to 24 moved one figure, a read's latency under a flat-out NT-write load (+6.7 ns), and
//! tRDWR 8 to 12 one other, the turnaround at matched load (+12 ns at 50%); the idle figures and every other Lat-* test moved with
//! neither. Lat-Loaded-DRAM measures a read's latency idle, under flat-out NT writes (its headline) and under flat-out reads.
//! Lat-Turnaround-DRAM, its matched levels, is the same function's other registration, to come with its pacing.
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
//! it spins with PAUSE in C0, so the idle rows run at the loaded rows' clocks.
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
/// Lat-Loaded-DRAM's ABBA pairs flat out when the config gives none, and the most it may give
pub const LOADED_PAIRS: u32 = 3;
pub const MAX_PAIRS: u32 = 50;
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

/// A loaded step's settings, from its config's parameters (`load_pairs`, `load_window_ms`) or the defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    pub pairs: usize,
    pub window: Duration,
}

impl Params {
    pub fn of(config: &TestMemoryConfig) -> Self {
        let context = config.parameter_context.as_ref();
        let pairs = context.and_then(|c| c.load_pairs).unwrap_or(LOADED_PAIRS).clamp(1, MAX_PAIRS);
        let window_ms = context.and_then(|c| c.load_window_ms).unwrap_or(DEFAULT_WINDOW_MS).clamp(1, MAX_WINDOW_MS);
        Params { pairs: pairs as usize, window: Duration::from_millis(window_ms.into()) }
    }

    /// The settle: `SETTLE`, or the window when it is shorter
    pub fn settle(&self) -> Duration {
        SETTLE.min(self.window)
    }

    /// The schedule a step with `loaders` loaders runs: the idle windows alone with none
    pub fn schedule(&self, loaders: usize) -> Schedule {
        if loaders == 0 { Schedule::idle(self.window, self.settle()) } else { Schedule::new(self.pairs, 0, self.window, self.settle()) }
    }
}

/// How long a loaded step of `config` on these CPUs takes, as its schedule says: the ticker's estimate
pub fn expected(config: &TestMemoryConfig, assignments: &[CpuAssignment]) -> Duration {
    let loaders = loaders_of(&assign_roles(assignments, crate::cpu_topology::get_physical_core_for_cpu));
    Duration::from_secs_f64(Params::of(config).schedule(loaders).expected_secs())
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
    pub chain_bytes: usize,
    pub width: crate::simd::Width,
    pub partial: bool,
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
    pub cell_bytes: usize,
    /// The loaders' kernels' width, in bits
    pub width: u32,
    pub block_bytes: usize,
    pub window_ms: f64,
    pub settle_ms: f64,
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

/// The table's rows from the raw windows: idle, each of `levels` levels, then flat out. `tails` gives a cell's (P50, P99) by its
/// histogram's index (`Window::cell`), when it has samples. Pure, so the statistic can be changed and rerun on windows already taken.
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
        cell_bytes: CELL,
        width: part.width.bits(),
        block_bytes: BLOCK,
        window_ms: part.schedule.window.as_secs_f64() * 1e3,
        settle_ms: part.schedule.settle.as_secs_f64() * 1e3,
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

/// A word on a 128 B line pair of its own, so the adjacent-line prefetcher doesn't pull it in with another
#[repr(align(128))]
struct Slot(AtomicU64);

/// One loaded step, shared by its threads: their roles, the barrier the participants (the measurer and the loaders) start at, the phase
/// word the measurer drives the schedule with, and each loader's running byte count.
pub struct Step {
    roles: Box<[Role]>,
    loaders: usize,
    measurer: Placement,
    barrier: Barrier,
    phase: Slot,
    slots: Box<[Slot]>,
    params: Params,
}

impl Step {
    /// A step on the threads of `assignments` (`ThreadPool::cpu_assignments`), with `config`'s settings
    pub fn new(assignments: &[CpuAssignment], config: &TestMemoryConfig) -> Self {
        Step::with_cores(assignments, crate::cpu_topology::get_physical_core_for_cpu, config)
    }

    /// `new`, a CPU's physical core from `core_of`
    fn with_cores(assignments: &[CpuAssignment], core_of: impl Fn(usize) -> usize, config: &TestMemoryConfig) -> Self {
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
            phase: Slot(AtomicU64::new(phase_word(0, Kind::Park))),
            slots: (0..loaders).map(|_| Slot(AtomicU64::new(0))).collect(),
            params: Params::of(config),
        }
    }

    /// Thread `thread`'s role: past the step's threads, sitting out
    fn role(&self, thread: usize) -> Role {
        self.roles.get(thread).copied().unwrap_or(Role::SitsOut)
    }

    /// Window `window`'s load, for the loaders (Release: what it carries is set before it)
    fn set_phase(&self, window: u64, kind: Kind) {
        self.phase.0.store(phase_word(window, kind), Ordering::Release);
    }

    /// Every loader's bytes so far
    fn loaded_bytes(&self) -> u64 {
        self.slots.iter().map(|s| s.0.load(Ordering::Relaxed)).sum()
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
        self.0.set_phase(0, Kind::Done);
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

/// One thread's part in a loaded step: its role's, chosen once.
///
/// # Safety
/// As the test functions'.
unsafe fn run(call: Call) -> LatencyTestStats {
    let alone;
    let (step, me) = match call.config.loaded.as_deref() {
        Some(step) => (step, call.thread_id),
        None => {
            alone = Step::with_cores(&[], |cpu| cpu, call.config);
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
    let mut records = Vec::with_capacity(schedule.windows.len());
    let mut partial = false;
    for (index, &window) in schedule.windows.iter().enumerate().take_while(|_| cells > 0) {
        step.set_phase(index as u64 + 1, window.kind);
        // The settle: the loaders onto their new load, untimed
        let settle_end = _rdtsc() + settle_ticks;
        loop {
            let (next, _, end) = crate::latency_tests::chase(ptr, READS);
            ptr = next;
            if end >= settle_end || runner.shutdown_requested() {
                break;
            }
        }
        let before = step.loaded_bytes();
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
        let after = step.loaded_bytes();
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
            target_mib_s: None,
            mean_ns: timed as f64 / (samples as usize * READS) as f64 / ghz,
            mib_s: (after - before) as f64 / MIB / secs,
            reads: samples * READS as u64,
            fell_behind: 0,
        };
        log::debug!(
            "[Thread {}] {} window {index} ({:?}): {:.1} ns over {} reads, the loaders at {:.0} MiB/s",
            call.thread_id,
            call.name,
            record.kind,
            record.mean_ns,
            record.reads,
            record.mib_s
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

/// A loader: fills its extent, waits for the others, then loads each window as the phase word says, from a random block each time,
/// until Done.
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

    let slot = &step.slots[index].0;
    let mut rng = crate::pattern::fmix(call.config.epoch.key() ^ crate::pattern::fmix(index as u64 + 1)) | 1;
    // Bytes moved, and of them those counted to the runner
    let (mut moved, mut counted, mut acc) = (0u64, 0u64, 0u64);
    let mut stopped = false;
    while blocks > 0 && !stopped {
        let phase = step.phase.0.load(Ordering::Acquire);
        let kernel = match kind_of(phase) {
            Kind::Done => break,
            Kind::Park => {
                park(&step.phase.0, phase);
                continue;
            }
            Kind::Writes => write,
            Kind::Reads => read,
        };
        // A new instance of the load: from a random block
        let mut at = crate::latency_tests::below(&mut rng, blocks);
        loop {
            let mut moved_on = false;
            for _ in 0..BLOCKS_A_CHECK {
                acc ^= kernel(base.add(at * BLOCK), BLOCK);
                moved += BLOCK as u64;
                slot.store(moved, Ordering::Relaxed);
                at += 1;
                if at == blocks {
                    at = 0;
                }
                if step.phase.0.load(Ordering::Relaxed) != phase {
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

        let idle = Params { pairs: 3, window: second }.schedule(0);
        assert_eq!(idle.windows, [Window::IDLE; 2]);
        // The settle is never longer than the window
        assert_eq!(Params { pairs: 3, window: Duration::from_millis(5) }.settle(), Duration::from_millis(5));
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

    /// The step's four threads on CPUs 0-3 (thread 1 the measurer's sibling), each on its own region, `during` run on the test's
    /// thread while they work; their results by thread
    fn run_step(window_ms: u32, during: impl FnOnce(&[std::thread::ScopedJoinHandle<'_, LatencyTestStats>])) -> Vec<LatencyTestStats> {
        let assignments = threads(&[0, 1, 2, 3], |_| 0);
        let step = Arc::new(Step::with_cores(&assignments, |cpu| STEP_CORES[cpu], &config(window_ms)));
        let config = TestMemoryConfig { loaded: Some(step), ..config(window_ms) };
        let regions: Vec<Region> = (0..4).map(|_| Region::new(&[256 * KIB])).collect();
        let stats = std::thread::scope(|s| {
            let handles: Vec<_> = regions
                .iter()
                .enumerate()
                .map(|(t, region)| {
                    let (blocks, config) = (&region.blocks, &config);
                    // SAFETY: each thread's own region, live for the scope
                    s.spawn(move || unsafe { lat_loaded_dram_multi(blocks, t, ErrorMode::Log, &config.timing, config, None) })
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
        let stats = run_step(20, |_| {});
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

    /// A thread that sits out returns at once, waiting for no one: alone here, it would wait forever at the participants' barrier
    #[test]
    fn a_sit_out_returns_at_once() {
        let assignments = threads(&[0, 1, 2, 3], |_| 0);
        let step = Arc::new(Step::with_cores(&assignments, |cpu| STEP_CORES[cpu], &config(5)));
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
        let stats = run_step(1000, |handles| {
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
