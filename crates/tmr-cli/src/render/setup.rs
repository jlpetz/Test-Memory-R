//! A session's setup and its end as the console shows them (TODO 29): each event the print group it replaced, moved here from the engine's
//! `cli.rs` (the CPU topology's from `cpu_topology.rs`, the memory status's from `runner.rs`) with its format strings. No ticker is up
//! during setup, so all print with a plain `console::print`, and to stderr what went there (`console::eprint`).

use std::fmt::Write as _;

use tmr::frontend::{
    CacheCalibrationState, ErrorMode, MemoryStatus, ParamNote, RunStatus, SealCalibrationState, Selection, Setup, core_list,
};
use tmr::session::{SessionEnd, Stopped};

use super::{rendered, report_text};
use crate::console;

/// Shows `setup`, and returns what it showed (the transcript's, TODO 109). From `Starting` on, Ctrl+C stops the run gracefully:
/// `take_ctrl_c` installs that (`crate::take_ctrl_c`; a test passes its own, as a test process keeps Ctrl+C for itself).
pub(super) fn show(setup: &Setup, take_ctrl_c: impl FnOnce()) -> String {
    let shown = shown(setup);
    match setup {
        Setup::MemoryStatus(None) => console::eprint(&shown),
        Setup::Starting => {
            console::print(&shown);
            take_ctrl_c();
        }
        _ => console::print(&shown),
    }
    shown
}

/// What the console shows for `setup`, on stdout or, for a memory status it couldn't read, on stderr
pub(super) fn shown(setup: &Setup) -> String {
    match setup {
        Setup::MemoryStatus(None) => "⚠️ Failed to retrieve system memory status\n".to_string(),
        setup => text(setup),
    }
}

/// What `setup` prints on stdout.
pub(super) fn text(setup: &Setup) -> String {
    let mut text = String::new();
    match setup {
        Setup::TestFilter { pattern } => {
            let _ = writeln!(text, "🎯 Test Filter: {}", pattern);
            let _ = writeln!(text, "  Running tests matching pattern");
            let _ = writeln!(text, "  Default: cycles=1 for quick targeted runs");
            let _ = writeln!(text, "  CLI overrides supported (e.g. duration=60 cycles=5)\n");
        }
        Setup::QuickTest => {
            let _ = writeln!(text, "🚀 TMR Quick Test Mode: plan=quick, the stability tests once");
            let _ = writeln!(text, "  Quick test uses reduced memory (60% reserve) for faster validation");
            let _ = writeln!(text, "  CLI overrides supported\n");
        }
        Setup::ParamNotes(notes) => {
            for note in notes {
                let _ = match note {
                    ParamNote::MinPage(page_str) => writeln!(text, "  Minimum Page Size: {}", page_str),
                    ParamNote::MaxPage(page_str) => writeln!(text, "  Maximum Page Size: {}", page_str),
                    ParamNote::Block(key, v) => writeln!(text, "  {}: {}", key, v),
                    ParamNote::Topology(method) => writeln!(text, "  Topology Detection: {}", method),
                    ParamNote::SkipCores(spec) => writeln!(text, "  CPU Pinning: skip-cores={}", spec),
                    ParamNote::PinningOff => writeln!(text, "  CPU Pinning: Disabled"),
                };
            }
        }
        Setup::StartupHeader { command_line, release } => {
            let _ = writeln!(text, "Startup Mode & Parameters:");
            let _ = writeln!(text, "  Command Line: {}", command_line.join(" "));

            // Display build type (helps identify performance issues in logs)
            let (build_type, build_icon) = if !release { ("Debug (unoptimized)", "❌") } else { ("Release (optimized)", "✅") };
            let _ = writeln!(text, "  Build Type: {} {}", build_icon, build_type);
        }
        Setup::StartupParams { rust_log, rust_backtrace, explicit, defaults } => {
            let _ = writeln!(text, "  RUST_LOG: {}", rust_log);
            if let Some(rust_backtrace) = rust_backtrace {
                let _ = writeln!(text, "  RUST_BACKTRACE: {}", rust_backtrace);
            }
            if !explicit.is_empty() {
                let _ = writeln!(text, "  Explicit Parameters: {}", explicit.join(", "));
            }
            if !defaults.is_empty() {
                let _ = writeln!(text, "  Default Parameters: {}", defaults.join(", "));
            }
        }
        Setup::CpuType { cputype, avoid_smt } => {
            if *avoid_smt {
                let _ = writeln!(text, "  CPU Type: {} (avoiding SMT/Hyperthreading)", cputype);
            } else {
                let _ = writeln!(text, "  CPU Type: {}", cputype);
            }
        }
        Setup::CpuCount { percent, available, threads } => {
            let _ = writeln!(text, "  CPU Count: {}% of {} available = {} thread(s)", percent, available, threads);
        }
        Setup::Selection(selection) => selection_text(&mut text, selection),
        Setup::LargePageCheck => {
            let _ = writeln!(text, "Checking Large Page access");
        }
        // After the question's pick, whose answer said it was granting (`prompts::large_pages`)
        Setup::LargePageGrant { account, outcome: Ok(()) } => {
            let _ = writeln!(
                text,
                "✅ Granted 'Lock pages in memory' (SeLockMemoryPrivilege) to {}, which keeps it until it is removed",
                account
            );
            let _ =
                writeln!(text, "🔄 SIGN-IN REQUIRED: sign out and back in (or restart Windows) for it to take effect, then run TMR again");
            let _ = writeln!(text);
            let _ = writeln!(text, "To sign out: Start Menu → your account → Sign out");
            let _ = writeln!(text, "To restart:");
            let _ = writeln!(text, "  • Windows: shutdown /r /t 0");
            let _ = writeln!(text, "  • Or use Start Menu → Power → Restart");
            let _ = writeln!(text);
            let _ = writeln!(text, "TMR will exit now.");
        }
        Setup::LargePageGrant { account, outcome: Err(e) } => {
            let _ = writeln!(text, "❌ Couldn't grant 'Lock pages in memory' (SeLockMemoryPrivilege) to {}: {}", account, e);
            let _ = writeln!(text);
            let _ = writeln!(text, "📖 Manual Setup:");
            let _ = writeln!(text, "1. Run gpedit.msc as Administrator (Windows Pro and up: Home has none)");
            let _ = writeln!(
                text,
                "2. Navigate to: Computer Configuration → Windows Settings → Security Settings → Local Policies → User Rights Assignment"
            );
            let _ = writeln!(text, "3. Double-click 'Lock pages in memory'");
            let _ = writeln!(text, "4. Add {}", account);
            let _ = writeln!(text, "5. Sign out and back in (or restart Windows)");
            let _ = writeln!(text);
        }
        Setup::LargePageDiagnostic { report } => {
            let _ = writeln!(text, "{}", report);
        }
        Setup::LargePageStatus { usable, min_page, min_needs_large } => {
            if *usable {
                let _ = writeln!(text, "🎉 Large Pages: ✅ ENABLED and tested successfully!");
                let _ = writeln!(text, "   TMR will use 2MB large pages for optimal performance");
            } else if *min_needs_large {
                // Without the privilege, a large minpage (the default) refuses the run at allocation
                let _ = writeln!(text, "⚠️  Large Pages: Not available, and minpage={} needs them, so the run will stop", min_page);
                let _ = writeln!(text, "   before allocating. Set them up (tmr.exe --setup-large-pages, elevated), or rerun");
                let _ = writeln!(text, "   with minpage=regular to test on standard 4KB pages: every test still runs, but");
                let _ = writeln!(text, "   slower memory traffic is less stress, so marginal errors that only appear at");
                let _ = writeln!(text, "   full bandwidth may go undetected");
            } else {
                let _ = writeln!(text, "⚠️  Large Pages: Not available - TMR will use standard 4KB pages");
                let _ = writeln!(text, "   Performance impact: ~5-10% slower memory allocation");
                let _ = writeln!(text, "   Every test still runs and every pattern is still checked, but slower");
                let _ = writeln!(text, "   memory traffic is less stress, so marginal errors that only appear at");
                let _ = writeln!(text, "   full bandwidth may go undetected");
            }

            let _ = writeln!(text);
        }
        Setup::CacheCalibration(state) => {
            let _ = match state {
                CacheCalibrationState::Loaded { at } => writeln!(text, "  Calibration: Loaded ({})", at.format("%Y-%m-%d %H:%M")),
                CacheCalibrationState::None => writeln!(text, "  Calibration: None (run --calibrate-cache to calibrate)"),
                CacheCalibrationState::Stale => writeln!(text, "  Calibration: Stale (hardware changed, re-run --calibrate-cache)"),
                CacheCalibrationState::Disabled => writeln!(text, "  Calibration: Disabled (--no-calibration)"),
            };
        }
        Setup::SealCalibration(state) => {
            let _ = match state {
                SealCalibrationState::Loaded { bits, at } => {
                    writeln!(text, "  Seal calibration: Loaded ({} bits, {})", bits, at.format("%Y-%m-%d %H:%M"))
                }
                SealCalibrationState::Stale => writeln!(text, "  Seal calibration: Stale (hardware changed, re-run --calibrate-seal)"),
                SealCalibrationState::None => writeln!(text, "  Seal calibration: None (run --calibrate-seal to calibrate)"),
            };
        }
        Setup::CpuTopology { report, smt_p_cores, e_cores_skipped_first } => {
            let (result, table) = rendered(|reporter| reporter.report_cpu_topology(report));
            text += &table;
            match result {
                Err(e) => log::error!("Failed to display CPU topology table: {}", e),
                // Additional SMT usage information for hybrid CPUs
                Ok(()) => {
                    if let Some(p_cores_with_smt) = smt_p_cores.filter(|&n| n > 0) {
                        let _ = writeln!(text, "  SMT/Hyperthreading: Enabled on {} P-cores", p_cores_with_smt);
                    }

                    if *e_cores_skipped_first {
                        let _ = writeln!(text, "    Note: E-cores were prioritized for skipping");
                    }
                }
            }
            let _ = writeln!(text);
        }
        Setup::MemoryStatus(Some(status)) => memory_status_text(&mut text, status),
        // On stderr (`show`)
        Setup::MemoryStatus(None) => {}
        Setup::SystemInfo { report, memory_modules, system, bios, hypervisor } => {
            text += &report_text("system info report", |reporter| reporter.report_system_info(report));
            machine_text(&mut text, system, bios, memory_modules, hypervisor.as_ref());
            let _ = writeln!(text);
        }
        Setup::Starting => {
            let _ = writeln!(text, "Starting comprehensive memory tests... Use CTRL+C for graceful shutdown with final report");
            let _ = writeln!(text, "(more here with RUST_LOG=info or debug; the log file takes info either way)");
            let _ = writeln!(text);
        }
        Setup::RealtimeOnEveryCpu => {
            let _ = writeln!(
                text,
                "⚠️  priority=realtime with a worker on every logical CPU: the console and input can freeze \
                      until the run ends. Leave a core free (skip-cores=1) or use priority=high."
            );
        }
        Setup::Plan { name, about, is_default } => {
            let default = if *is_default { " (the default)" } else { "" };
            let _ = writeln!(text, "📋 Plan: {}{default}: {}", name, about);
        }
    }
    text
}

/// The machine as SMBIOS and CPUID give it (TODO 105, the owner 2026-11-04): a line for the system, its BIOS, its UUID and its memory; one,
/// indented, for each DIMM, its serial too; and one for a hypervisor, a VM's or the one VBS runs on (TODO 115). Under `--no-serials` the
/// serials and the UUID aren't there, and the system's line says so.
fn machine_text(
    text: &mut String, system: &tmr::frontend::SystemIdentity, bios: &tmr::frontend::BiosInfo, modules: &[tmr::frontend::MemoryModule],
    hypervisor: Option<&tmr::frontend::Hypervisor>,
) {
    let gib = |bytes: u64| {
        let gib = bytes as f64 / (1u64 << 30) as f64;
        if gib.fract() == 0.0 { format!("{gib:.0} GiB") } else { format!("{gib:.1} GiB") }
    };
    let joined = |parts: &[&str]| parts.iter().filter(|part| !part.trim().is_empty()).map(|part| part.trim()).collect::<Vec<_>>().join(" ");
    let mut parts = Vec::new();
    let name = joined(&[&system.manufacturer, &system.product_name]);
    parts.push(if name.is_empty() { "unnamed".to_string() } else { name });
    let firmware = joined(&[&bios.vendor, &bios.version]);
    if !firmware.is_empty() {
        let date = if bios.release_date.is_empty() { String::new() } else { format!(" ({})", bios.release_date) };
        parts.push(format!("BIOS {firmware}{date}"));
    }
    if !system.uuid.is_empty() {
        parts.push(format!("UUID {}", system.uuid));
    }
    let total: u64 = modules.iter().map(|module| module.capacity_bytes).sum();
    parts.push(match modules.len() {
        0 => "no memory modules listed".to_string(),
        1 => format!("1 module, {}", gib(total)),
        n => format!("{n} modules, {}", gib(total)),
    });
    if tmr::frontend::serials_hidden() {
        parts.push("serials left out (--no-serials)".to_string());
    }
    let _ = writeln!(text, "🖥️  System: {}", parts.join(" · "));
    for module in modules {
        let size = joined(&[&gib(module.capacity_bytes), tmr::frontend::memory_type_name(module.memory_type).unwrap_or("")]);
        let speed = if module.speed_mts > 0 { format!("{} MT/s", module.speed_mts) } else { String::new() };
        let maker = joined(&[&module.manufacturer, &module.part_number]);
        let serial = if module.serial_number.trim().is_empty() { String::new() } else { format!("serial {}", module.serial_number.trim()) };
        let fields = [joined(&[&size, &speed]), maker, serial];
        let details: Vec<&str> = fields.iter().filter(|field| !field.is_empty()).map(String::as_str).collect();
        let locator = if module.locator.trim().is_empty() { "a module" } else { module.locator.trim() };
        let _ = writeln!(text, "    {locator}: {}", details.join(" · "));
    }
    match hypervisor {
        Some(tmr::frontend::Hypervisor { name, vm_signs }) if vm_signs.is_empty() => {
            let _ = writeln!(text, "🧩 Hypervisor: {name}, the one Windows runs on (VBS or Hyper-V on this PC): not a VM");
        }
        Some(tmr::frontend::Hypervisor { name, vm_signs }) => {
            let why = vm_signs.join(", ");
            let _ = writeln!(text, "🧩 Hypervisor: {name}, a VM ({why}): without a calibration, the L3 tests take half the L3's share");
        }
        None => {}
    }
}

/// The CPUs chosen, and the run's error mode, limits and SIMD, then a blank line.
fn selection_text(text: &mut String, selection: &Selection) {
    let Selection {
        error_mode,
        threads,
        available,
        cputype,
        cpu_list,
        pinned,
        skip,
        skipped,
        avoid_smt_doubling,
        cycles,
        duration_secs,
        simd,
        // The window's: the console's thread stays as it is
        window_priority: _,
    } = selection;
    text.push_str("  Error Mode: ");
    let _ = match error_mode {
        ErrorMode::Log => writeln!(text, "Log and continue"),
        ErrorMode::Halt => writeln!(text, "Halt on first error"),
        ErrorMode::Panic => writeln!(text, "Panic on error (debug mode)"),
    };

    let _ = writeln!(text, "  Using {}/{} {} for testing", threads, available, cputype);
    if *pinned {
        let _ = writeln!(text, "  CPU Assignment: {:?}", cpu_list);
    } else {
        let _ = writeln!(text, "  CPU Assignment: {:?} ⚠️ NUMA placement only, pinning is off", cpu_list);
    }
    // The cores skip-cores= left out: from the end of the core order, or a range of core numbers (TODO 108)
    if !skipped.is_empty() && skip.from_the_end() {
        let _ = writeln!(
            text,
            "  Skipping {}, the last in the core order (skip-cores={}), for system responsiveness",
            core_list(skipped),
            skip
        );
    } else if !skipped.is_empty() {
        let _ = writeln!(text, "  Skipping {} (skip-cores={})", core_list(skipped), skip);
    }
    if *avoid_smt_doubling {
        let _ = writeln!(text, "  Avoiding SMT doubling (using physical cores only)");
    }

    // Display timing configuration
    text.push_str("  Test Suite Timing: ");
    let _ = match (cycles, duration_secs) {
        (Some(cycles), Some(duration)) => writeln!(text, "{} cycles or {}s max (whichever first)", cycles, duration),
        (Some(cycles), None) => writeln!(text, "{} cycles (no time limit)", cycles),
        (None, Some(duration)) => writeln!(text, "{}s duration (unlimited cycles)", duration),
        (None, None) => writeln!(text, "Unlimited cycles and duration"),
    };
    let _ = writeln!(text, "  SIMD Support: {}", simd);
    let _ = writeln!(text);
}

/// The system's memory as a table, then a blank line.
fn memory_status_text(text: &mut String, status: &MemoryStatus) {
    use crate::table::{Alignment, TableBuilder};

    let _ = writeln!(text, "📊 Current System Memory Status:");

    let total_phys_gib = status.total_phys as f64 / (1024.0 * 1024.0 * 1024.0);
    let avail_phys_gib = status.avail_phys as f64 / (1024.0 * 1024.0 * 1024.0);
    let total_page_gib = status.total_page_file as f64 / (1024.0 * 1024.0 * 1024.0);
    let avail_page_gib = status.avail_page_file as f64 / (1024.0 * 1024.0 * 1024.0);
    let phys_percent = (status.avail_phys as f64 / status.total_phys as f64) * 100.0;

    let table = TableBuilder::new()
        .add_header("Memory Type", Alignment::Left)
        .add_header("Total", Alignment::Right)
        .add_header("Available", Alignment::Right)
        .add_header("Used", Alignment::Right)
        .add_header("% Available", Alignment::Right)
        .add_row(vec![
            "Physical".to_string(),
            format!("{:.2} GiB", total_phys_gib),
            format!("{:.2} GiB", avail_phys_gib),
            format!("{:.2} GiB", total_phys_gib - avail_phys_gib),
            format!("{:.1}%", phys_percent),
        ])
        .add_row(vec![
            "Page File".to_string(),
            format!("{:.2} GiB", total_page_gib),
            format!("{:.2} GiB", avail_page_gib),
            format!("{:.2} GiB", total_page_gib - avail_page_gib),
            format!("{:.1}%", (avail_page_gib / total_page_gib) * 100.0),
        ]);

    let _ = table.write_to(text);
    let _ = writeln!(text);
}

/// How the session ended, as the console says it, `program` the exe's name for the help an unknown flag shows. A command's output is its
/// own (`tmr::commands::run`), and the large-page sign-out's text was the prompt's.
pub(crate) fn show_end(end: &SessionEnd, program: &str) {
    match end {
        SessionEnd::Stopped(Stopped::Invalid(e)) => console::eprint(&format!("{}\n", e)),
        SessionEnd::Stopped(Stopped::CpuSelection(e)) => console::eprint(&format!("❌ CPU selection error: {}\n", e)),
        end => console::print(&end_text(end, program)),
    }
}

/// What `end` prints on stdout.
fn end_text(end: &SessionEnd, program: &str) -> String {
    let mut text = String::new();
    match end {
        SessionEnd::Ran { status, elapsed, plan_only } => {
            if *plan_only {
                if *status == RunStatus::Passed {
                    let _ = writeln!(text, "\n================================================================================");
                    let _ = writeln!(
                        text,
                        "  --startup-debug: Startup sequence and test plan complete. No test memory allocated, no test run."
                    );
                    let _ = writeln!(text, "================================================================================");
                }
            } else {
                let _ = writeln!(text);
                let _ = writeln!(text, "================================================================================");
                let _ = match status {
                    RunStatus::Passed => writeln!(text, "✅ All memory tests completed successfully in {}", format_duration(*elapsed)),
                    RunStatus::Failed => writeln!(text, "❌ Tests failed or encountered errors in {}", format_duration(*elapsed)),
                    RunStatus::NotRun => writeln!(text, "❌ Stopped before testing: see the message above"),
                    RunStatus::Interrupted => {
                        writeln!(text, "⚠️  Interrupted after {}: only the tests that finished were checked", format_duration(*elapsed))
                    }
                };
            }
        }
        SessionEnd::Stopped(Stopped::UnknownFlags(flags)) => {
            let _ = writeln!(text, "❌ Error: Unknown parameter(s) detected:");
            for flag in flags {
                let _ = writeln!(text, "  {}", flag);
            }
            let _ = writeln!(text);
            let _ = writeln!(text, "Note: --latency-test has been renamed to --ram-latency");
            let _ = writeln!(text);
            let mut help = Vec::new();
            let _ = tmr::commands::help(&mut help, program);
            text += &String::from_utf8_lossy(&help);
        }
        // On stderr (`show_end`)
        SessionEnd::Stopped(Stopped::Invalid(_) | Stopped::CpuSelection(_)) => {}
        // The large-page question's quit, no answer to it, or a failed grant (TODO 113): nothing allocated, nothing slower run unasked
        SessionEnd::Stopped(Stopped::NeedsLargePages) => {
            let _ = writeln!(text);
            let _ = writeln!(text, "================================================================================");
            let _ = writeln!(text, "❌ Stopped before allocating: this sign-in lacks the right large pages need, and TMR runs nothing");
            let _ = writeln!(text, "   on 4KB pages unasked (minpage=regular maxpage=regular runs on them without asking)");
        }
        // Said already: the sign-out by the prompt
        SessionEnd::SignOutChosen | SessionEnd::Command(_) => {}
    }
    text
}

fn format_duration(duration: std::time::Duration) -> String {
    // Format runtime as HH:MM:SS
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}
