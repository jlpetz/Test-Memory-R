//! The console's startup sections, as the window's tabs show them (TODO 109): System (the CPU, its caches and their calibration, the TSC,
//! the memory modules), Threads (the CPUs chosen, the cores with each thread's CPU, and at the run's end its work by thread, CPU and core)
//! and Memory (Windows' memory, the large pages, the memory plan, each thread's allocation). Each section is the console's: its reports the
//! engine's tables (`report::blocks`), its lines in the window's words, in the order the session sent them. Plain Rust, as `model.rs`.

use tmr::frontend::{
    CacheCalibrationState, ColumnAlignment, ErrorMode, MemoryModule, MemoryStatus, Run, SealCalibrationState, Selection, Setup, TableData,
};

use crate::report::{Block, blocks};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The tabs' sections, empty until the session's setup sends them
#[derive(Debug, Default)]
pub struct Sections {
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
            Setup::CpuFilter { lo, hi } => self.threads.push(text(format!("CPU filter: excluding cores {lo}-{hi}"))),
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
            Setup::SystemInfo { report, memory_modules } => {
                self.system.extend(blocks(|r| r.report_system_info(report)));
                self.system.push(Block::Table(modules_table(memory_modules)));
            }
            Setup::CacheCalibration(state) => self.system.push(text(match state {
                CacheCalibrationState::Loaded { extended, at } => {
                    let kind = if *extended { "extended" } else { "standard" };
                    format!("Cache calibration: loaded ({kind}, {})", at.format("%Y-%m-%d %H:%M"))
                }
                CacheCalibrationState::None => "Cache calibration: none (tmr.exe --calibrate-cache measures it)".to_string(),
                CacheCalibrationState::Stale => "Cache calibration: stale, the hardware changed (tmr.exe --calibrate-cache)".to_string(),
                CacheCalibrationState::Disabled => "Cache calibration: off (--no-calibration)".to_string(),
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
            Setup::TestFilter { .. }
            | Setup::QuickTest
            | Setup::ParamNotes(_)
            | Setup::StartupHeader { .. }
            | Setup::StartupParams { .. }
            | Setup::LargePageCheck
            | Setup::MemoryStatus(None)
            | Setup::Starting
            | Setup::RealtimeOnEveryCpu
            | Setup::Plan { .. } => {}
        }
    }

    /// What `run` adds to a section, if anything
    pub fn run(&mut self, run: &Run) {
        match run {
            Run::MemoryPlan(plan) => self.memory.extend(blocks(|r| r.report_consolidated_memory(plan))),
            Run::Channels { channels, cache_line } => {
                self.memory.push(Block::Text(format!("Memory channels: {channels}, cache line {cache_line} B")));
            }
            Run::Allocated(report) => {
                self.memory.push(Block::Heading(format!("Allocation · {} · {:.1} s", report.allocator, report.elapsed.as_secs_f64())));
                self.memory.extend(blocks(|r| r.report_allocation(report)));
            }
            Run::CpuPerformance(summary) => {
                self.threads.push(Block::Heading("The run's work, by thread, CPU and core".to_string()));
                self.threads.extend(blocks(|r| {
                    if !summary.by_thread.threads.is_empty() {
                        r.report_performance_by_thread(&summary.by_thread)?;
                    }
                    if !summary.by_cpu.cpus.is_empty() {
                        r.report_performance_by_cpu(&summary.by_cpu)?;
                    }
                    if !summary.by_core.cores.is_empty() {
                        r.report_performance_by_physical_core(&summary.by_core)?;
                    }
                    Ok(())
                }));
            }
            _ => {}
        }
    }
}

/// The CPUs chosen, and the run's error mode, limits and SIMD: the console's selection lines
fn selection_lines(s: &Selection) -> Vec<String> {
    let mut lines = vec![format!("Using {}/{} {} for testing", s.threads, s.available, s.cputype)];
    let unpinned = if s.pinned { "" } else { " (NUMA placement only: pinning is off)" };
    lines.push(format!("CPUs: {}{unpinned}", s.cpu_list.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")));
    if s.cpus_to_skip > 0 {
        lines.push(format!("Skipping the first {} CPU(s), for the system's responsiveness", s.cpus_to_skip));
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
            crate::sysinfo::memory_type(m.memory_type).unwrap_or_default().to_string(),
            if m.speed_mts > 0 { format!("{} MT/s", m.speed_mts) } else { String::new() },
            m.manufacturer.trim().to_string(),
            m.part_number.trim().to_string(),
        ])
    })
}
