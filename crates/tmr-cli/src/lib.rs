//! The command-line front end (TODO 29): the console's logging, a session shown through the CLI's
//! frontend (`render`), and how it ended: the verdict, why setup stopped it, or the one-shot command
//! it named, run on the console. The session itself is the engine's (`tmr::session::run`). Its own
//! crate since S5 of TODO 29, so the engine prints nothing, and the window (`crates/tmr-gui`)
//! doesn't name it: it has a logger of its own, which writes the same log file (`tmr::logfile`).

// The console goldens capture what a test prints as libtest does, and hand the capture on to the
// threads a test spawns as it does (`golden.rs`, TODO 29)
#![cfg_attr(test, feature(internal_output_capture, thread_spawn_hook))]

mod console; // single writer path while the progress ticker is on screen
#[cfg(test)]
mod golden;
mod prompts; // the CLI's questions, over any reader and writer (TODO 29)
mod render; // the CLI's frontend: a session's events as the console shows them (TODO 29)
mod renderer; // the engine's reports as the console shows them
mod table; // the console's output against goldens, for the engine/presentation split (TODO 29)

use std::io::{Write, stdin, stdout};
use std::sync::Arc;

use env_logger::Builder;
use tmr::frontend::RunStatus;
use tmr::session::SessionEnd;

/// What the engine's reports and the CLI's renders return: the engine's `Result`
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The CLI: a session on the console, `args` the command line (the exe's name first) less tmr.exe's
/// own `--cli`/`--gui`. Returns the exit status for scripts: 0 passed, 1 failed or found errors, 2
/// stopped before testing, 3 interrupted (`RunStatus::exit_code`). Any error stops TMR before
/// testing, so it exits 2 like every other "stopped before testing" case (`RunStatus::NotRun`);
/// the console gets it once, the log file too.
pub fn main(args: Vec<String>) -> i32 {
    // Initialize enhanced logging with file output
    setup_logging();

    println!("🚀 Test Memory R (TMR) v{} - High-Performance Memory Testing Tool", tmr::BUILD.version);
    println!("================================================================================");

    let program = args.first().cloned().unwrap_or_default();
    match tmr::session::run(args, Arc::new(crate::render::CliFrontend::new())) {
        Ok(end) => finish(&end, &program),
        Err(e) => {
            report_error(e.as_ref());
            RunStatus::NotRun.exit_code()
        }
    }
}

/// The error that stopped a session, as the CLI reports it: `Error: ...` on the console once, and
/// the error in the log file.
fn report_error(e: &dyn std::error::Error) {
    log::error!(target: tmr::logfile::FILE_ONLY_TARGET, "{e}");
    eprintln!("Error: {e}");
}

/// How the session ended, on the console (`program` the exe's name, for the help an unknown flag
/// shows), and its exit status. A one-shot command runs here.
fn finish(end: &SessionEnd, program: &str) -> i32 {
    let status = match end {
        SessionEnd::Command(command) => return run_command(command),
        SessionEnd::Ran { status, .. } => *status,
        // Nothing ran, as the user chose
        SessionEnd::RestartChosen => RunStatus::Passed,
        SessionEnd::Stopped(_) => RunStatus::NotRun,
    };
    crate::render::show_end(end, program);
    status.exit_code()
}

/// A one-shot command on the console, which a calibration asks whether to save what it measured.
/// Its exit status: 0 done, 2 failed.
fn run_command(command: &tmr::commands::Command) -> i32 {
    let mut confirm = |question: &str| crate::prompts::confirm(&mut stdin().lock(), &mut stdout(), question);
    match tmr::commands::run(command, &mut stdout(), &mut confirm) {
        Ok(status) => status.exit_code(),
        // The console can't be written to
        Err(e) => {
            report_error(&e);
            RunStatus::NotRun.exit_code()
        }
    }
}

/// Ctrl+C from the tests' start (`Setup::Starting`): a first press stops the run gracefully, a
/// second exits. Once a process: a second handler can't be set.
pub(crate) fn take_ctrl_c() {
    // Register CTRL+C handler for graceful shutdown
    // This allows tests to stop at next checkpoint and print final reports
    // A flag of the handler's own, not the engine's SHUTDOWN_REQUESTED: a halt sets that too, and
    // the first CTRL+C after a halt must still be the graceful one.
    let mut pressed = false;
    ctrlc::set_handler(move || {
        if pressed {
            // Second press: the graceful stop is stuck or too slow. Windows frees every page the
            // process holds, large pages included, when it exits. stderr rather than print_above,
            // because a stuck shutdown may be one that is holding the console lock.
            eprintln!("\n\n🛑 CTRL+C again - exiting now, without the final report; the results file keeps only the cycles saved so far");
            log::logger().flush();
            // The status Windows itself gives a process killed by CTRL+C, so a script can tell this apart
            // from a run that finished.
            std::process::exit(windows::Win32::Foundation::STATUS_CONTROL_C_EXIT.0);
        }
        pressed = true;
        // Above the progress ticker, which this thread would otherwise print into the middle of.
        crate::console::print_above(
            "\n🛑 CTRL+C received - initiating graceful shutdown...\n\
			 \x20  Tests will stop at next checkpoint and print final report\n\
			 \x20  (Press CTRL+C again to exit immediately, without the report; the results file keeps only the cycles saved so far)\n\n",
        );
        tmr::request_stop();
    })
    .expect("Error setting CTRL+C handler");
}

/// The console's logging: VT escape processing, then env_logger, each record on the console in
/// colour above the ticker and in the log file (`tmr::logfile`).
fn setup_logging() {
    // VT escape processing, for the log colours and the progress ticker (see console.rs).
    crate::console::init();

    // Create logs directory if it doesn't exist
    if let Err(e) = tmr::logfile::create_dir() {
        eprintln!("Warning: Failed to create logs directory: {}", e);
    }

    // Shared with the result filename so a run's log and result can be paired by name
    let log_filename = tmr::logfile::path();

    // The records the log file gets, as the window's logger has them too
    let (log_level, level_filter) = tmr::logfile::level();

    // Initialize file logger for dual logging
    tmr::logfile::open(&log_filename);

    // Set up env_logger for console with original nice formatting (colors + module info)
    Builder::new()
        .filter_level(level_filter)
        .format(|buf, record| {
            // Write to console with original env_logger format. `LogSink` below prints it above
            // the progress ticker, so the two never merge. A file-only record writes nothing here,
            // and `LogSink` drops an empty write.
            let console_result = if record.target() == tmr::logfile::FILE_ONLY_TARGET {
                Ok(())
            } else {
                writeln!(
                    buf,
                    "\x1b[{}m[{} {} {}]\x1b[0m {}",
                    match record.level() {
                        log::Level::Error => "31", // Red
                        log::Level::Warn => "33",  // Yellow
                        log::Level::Info => "32",  // Green
                        log::Level::Debug => "36", // Cyan
                        log::Level::Trace => "35", // Magenta
                    },
                    // Plain local time, no zone marker. This used to be `…%SZ` on a `Local::now()` —
                    // printing local while claiming UTC, which was the actual bug. A bare stamp claims
                    // nothing, which is both honest and easier to read; the zone is stated once in the
                    // log file's header for anything that gets archived. Matches the file format.
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    record.level(),
                    record.module_path().unwrap_or("unknown"),
                    record.args()
                )
            };

            // Also write to file without colors
            tmr::logfile::write(record);

            console_result
        })
        // Through the console module, so a log line can never land inside the progress ticker.
        // A pipe target gets no colour detection of its own (env_logger would strip the escapes
        // above), so decide here as the stdout target used to: colours on a console, none when
        // redirected.
        .target(env_logger::Target::Pipe(Box::new(crate::console::LogSink)))
        .write_style(if std::io::IsTerminal::is_terminal(&stdout()) {
            env_logger::WriteStyle::Always
        } else {
            env_logger::WriteStyle::Never
        })
        .init();

    // Log startup info
    log::info!("TMR v{} started - console logging with colors and module info restored", tmr::BUILD.version);
    log::info!("Log level: {} - detailed logs also saved to {}", log_level, log_filename);
}
