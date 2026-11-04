//! The console's startup sections, as the window's tabs show them (TODO 109): Plan (the plan, its tests' configuration, the memory
//! channels, the patterns' seed), System (the CPU, its caches and their calibration, the TSC, the memory modules), Threads (the CPUs
//! chosen, the cores with each thread's CPU) and Memory (Windows' memory, the large pages, the memory plan, each thread's allocation). All
//! of it is setup: a section doesn't change once the suite has started, and the run's work by thread goes in the final summary's report.
//! Each section is the console's: its reports the engine's tables (`report::blocks`), its lines in the window's words, in the order the
//! session sent them. Plain Rust, as `model.rs`.

use tmr::frontend::{
    CacheCalibrationState, ColumnAlignment, ErrorMode, MemoryModule, MemoryStatus, Run, SealCalibrationState, Selection, Setup, TableData,
    core_list,
};

use crate::report::{Block, blocks};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The tabs' sections, empty until the session's setup sends them
#[derive(Debug, Default)]
pub struct Sections {
    pub plan: Vec<Block>,
    pub system: Vec<Block>,
    pub threads: Vec<Block>,
    pub memory: Vec<Block>,
}

impl Sections {
    /// What `setup` adds to a section, if anything
    pub fn setup(&mut self, setup: &Setup) {
        let text = |text: String| Block::Text(text);
        match setup {
            Setup::CpuType { cputype, avoid_smt } => {
                let smt = if *avoid_smt { ", physical cores only (avoiding SMT doubling)" } else { "" };
                self.threads.push(text(format!("CPU type: {cputype}{smt}")));
            }
            Setup::CpuCount { percent, available, threads } => {
                self.threads.push(text(format!("CPU count: {percent}% of {available} available = {threads} thread(s)")));
            }
            Setup::Selection(selection) => self.threads.extend(selection_lines(selection).into_iter().map(text)),
            Setup::CpuTopology { report, smt_p_cores, e_cores_skipped_first } => {
                self.threads.extend(blocks(|r| r.report_cpu_topology(report)));
                if let Some(cores) = smt_p_cores.filter(|&cores| cores > 0) {
                    self.threads.push(text(format!("SMT: on, on {cores} P-cores")));
                }
                if *e_cores_skipped_first {
                    self.threads.push(text("E-cores were skipped first".to_string()));
                }
            }
            Setup::SystemInfo { report, memory_modules, .. } => {
                self.system.extend(blocks(|r| r.report_system_info(report)));
                self.system.push(Block::Table(modules_table(memory_modules)));
            }
            Setup::CacheCalibration(state) => self.system.push(text(match state {
                CacheCalibrationState::Loaded { at } => format!("Cache calibration: loaded ({})", at.format("%Y-%m-%d %H:%M")),
                CacheCalibrationState::Measuring(why) => format!("Cache calibration: {}", measuring(*why)),
                CacheCalibrationState::Rechecking { at } => {
                    format!("Cache calibration: loaded ({}), rechecked before the tests (calibrate=recheck)", at.format("%Y-%m-%d %H:%M"))
                }
                CacheCalibrationState::Disabled => "Cache calibration: off (calibrate=off): sizes from CPUID".to_string(),
            })),
            Setup::SealCalibration(state) => self.system.push(text(match state {
                SealCalibrationState::Loaded { bits, at } => {
                    format!("Seal calibration: loaded ({bits} bits, {})", at.format("%Y-%m-%d %H:%M"))
                }
                SealCalibrationState::Stale => "Seal calibration: stale, the hardware changed (tmr.exe --calibrate-seal)".to_string(),
                SealCalibrationState::None => "Seal calibration: none (tmr.exe --calibrate-seal measures it)".to_string(),
            })),
            Setup::MemoryStatus(Some(status)) => self.memory.push(Block::Table(memory_status_table(status))),
            Setup::LargePageDiagnostic { report } => self.memory.push(text(report.trim_end().to_string())),
            Setup::LargePageGrant { account, outcome } => self.memory.push(match outcome {
                Ok(()) => text(format!(
                    "Large pages: 'Lock pages in memory' granted to {account}, until it is removed; it takes effect at the next sign-in"
                )),
                Err(e) => Block::Warning(format!("Large pages: 'Lock pages in memory' couldn't be granted to {account}: {e}")),
            }),
            Setup::LargePageStatus { usable, min_page, min_needs_large } => self.memory.push(if *usable {
                text("Large pages: on, tested".to_string())
            } else if *min_needs_large {
                Block::Warning(format!(
                    "Large pages: not available, and minpage={min_page} needs them, so the run stops before allocating: \
                     tmr.exe --setup-large-pages (elevated), or minpage=regular for 4 KiB pages"
                ))
            } else {
                Block::Warning("Large pages: not available, so the run takes 4 KiB pages: less stress, about 5-10% slower".to_string())
            }),
            Setup::Plan { name, about, is_default } => {
                let default = if *is_default { " (the default)" } else { "" };
                self.plan.push(text(format!("Plan: {name}{default}: {about}")));
            }
            Setup::TestFilter { pattern } => self.plan.push(text(format!("Test filter: {pattern}"))),
            Setup::QuickTest
            | Setup::ParamNotes(_)
            | Setup::StartupHeader { .. }
            | Setup::StartupParams { .. }
            | Setup::LargePageCheck
            | Setup::MemoryStatus(None)
            | Setup::Starting
            | Setup::RealtimeOnEveryCpu => {}
        }
    }

    /// What `run` adds to a section, if anything
    pub fn run(&mut self, run: &Run) {
        match run {
            Run::MemoryPlan(plan) => self.memory.extend(blocks(|r| r.report_consolidated_memory(plan))),
            Run::Channels { channels, cache_line } => self.plan.push(Block::Text(format!(
                "Memory channels: {channels}, which set SimpleTest's stride: (channels × parameter − 1) × the {cache_line} B cache line"
            ))),
            Run::TestPlan(plan) => self.plan.extend(blocks(|r| r.report_test_configuration(plan))),
            Run::SuiteStarted { seed, transfer_bits, transfer_source, .. } => {
                let width = transfer_bits
                    .map_or("unknown, so switch alternates 64 and 32 bits".to_string(), |bits| format!("{bits} bits ({transfer_source})"));
                self.plan.push(Block::Text(format!("Pattern seed {seed:#018x} (seed= replays it), transfer width {width}")));
            }
            // Its first table's title names the allocator and its time
            Run::Allocated(report) => self.memory.extend(blocks(|r| r.report_allocation(report))),
            // TODO 22: the calibration the run measured before its tests
            Run::Calibrated(Ok(now)) => {
                self.system.push(Block::Text(now.table.trim().to_string()));
                self.system.push(Block::Text(match &now.saved {
                    Ok(path) => format!("Cache calibration: measured now, saved to {}", path.display()),
                    Err(e) => format!("Cache calibration: measured now, not saved ({e})"),
                }));
            }
            Run::Calibrated(Err(e)) => self.system.push(Block::Text(format!("Cache calibration: failed ({e}), sizes from CPUID"))),
            Run::Rechecked(Ok(recheck)) => self.system.push(Block::Text(recheck.text().trim_end().to_string())),
            Run::Rechecked(Err(e)) => self.system.push(Block::Text(format!("Cache calibration: the recheck failed ({e}), measured again"))),
            _ => {}
        }
    }
}

/// The CPUs chosen, and the run's error mode, limits and SIMD: the console's selection lines
/// Why the run measures the cache calibration before its tests (TODO 22), as the console says it
fn measuring(why: tmr::frontend::MeasureWhy) -> &'static str {
    use tmr::frontend::MeasureWhy;
    match why {
        MeasureWhy::NoneStored => "none stored, measured before the tests (calibrate=auto)",
        MeasureWhy::OtherHardware => "the one stored is another machine's, measured before the tests (calibrate=auto)",
        MeasureWhy::OneThreadOnly => "the one stored measured one thread only, measured again before the tests (calibrate=auto)",
        MeasureWhy::Always => "measured before the tests (calibrate=always)",
    }
}

fn selection_lines(s: &Selection) -> Vec<String> {
    let mut lines = vec![format!("Using {}/{} {} for testing", s.threads, s.available, s.cputype)];
    let unpinned = if s.pinned { "" } else { " (NUMA placement only: pinning is off)" };
    lines.push(format!("CPUs: {}{unpinned}", s.cpu_list.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")));
    if !s.skipped.is_empty() && s.skip.from_the_end() {
        lines.push(format!(
            "Skipping {}, the last in the core order (skip-cores={}), for the system's responsiveness",
            core_list(&s.skipped),
            s.skip
        ));
    } else if !s.skipped.is_empty() {
        lines.push(format!("Skipping {} (skip-cores={})", core_list(&s.skipped), s.skip));
    }
    if s.avoid_smt_doubling {
        lines.push("Physical cores only (avoiding SMT doubling)".to_string());
    }
    lines.push(format!("Error mode: {}", match s.error_mode {
        ErrorMode::Log => "log and continue",
        ErrorMode::Halt => "halt on the first error",
        ErrorMode::Panic => "panic on an error (debug)",
    }));
    lines.push(format!("Limits: {}", match (s.cycles, s.duration_secs) {
        (Some(cycles), Some(secs)) => format!("{cycles} cycles or {secs} s, whichever comes first"),
        (Some(cycles), None) => format!("{cycles} cycles, no time limit"),
        (None, Some(secs)) => format!("{secs} s, no cycle limit"),
        (None, None) => "none".to_string(),
    }));
    lines.push(format!("SIMD: {}", s.simd));
    lines
}

/// Windows' memory, physical and the page file, as the console's table has it
fn memory_status_table(status: &MemoryStatus) -> TableData {
    let row = |name: &str, total: u64, available: u64| {
        let (total, available) = (total as f64 / GIB, available as f64 / GIB);
        let percent = if total > 0.0 { available / total * 100.0 } else { 0.0 };
        vec![
            name.to_string(),
            format!("{total:.2} GiB"),
            format!("{available:.2} GiB"),
            format!("{:.2} GiB", total - available),
            format!("{percent:.1}%"),
        ]
    };
    TableData::new()
        .with_title("Windows' memory")
        .add_header("Memory", ColumnAlignment::Left)
        .add_header("Total", ColumnAlignment::Right)
        .add_header("Available", ColumnAlignment::Right)
        .add_header("Used", ColumnAlignment::Right)
        .add_header("% Available", ColumnAlignment::Right)
        .add_row(row("Physical", status.total_phys, status.avail_phys))
        .add_row(row("Page file", status.total_page_file, status.avail_page_file))
}

/// The memory modules SMBIOS lists, one a row. Not their serial numbers: a window is shared as a screenshot, and the results file has them.
fn modules_table(modules: &[MemoryModule]) -> TableData {
    let table = TableData::new()
        .with_title("Memory modules (SMBIOS)")
        .add_header("Locator", ColumnAlignment::Left)
        .add_header("Size", ColumnAlignment::Right)
        .add_header("Type", ColumnAlignment::Left)
        .add_header("Speed", ColumnAlignment::Right)
        .add_header("Maker", ColumnAlignment::Left)
        .add_header("Part", ColumnAlignment::Left);
    modules.iter().fold(table, |table, m| {
        table.add_row(vec![
            m.locator.clone(),
            if m.capacity_bytes > 0 { crate::sysinfo::size(m.capacity_bytes as usize) } else { String::new() },
            tmr::frontend::memory_type_name(m.memory_type).unwrap_or_default().to_string(),
            if m.speed_mts > 0 { format!("{} MT/s", m.speed_mts) } else { String::new() },
            m.manufacturer.trim().to_string(),
            m.part_number.trim().to_string(),
        ])
    })
}
