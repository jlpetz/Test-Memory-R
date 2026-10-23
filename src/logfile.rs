//! The log file (TODO 29): `logs/<stem>.log`, with the stem the run's results file shares, a header, then one line per record, `[stamp
//! LEVEL module] message`, in local time to the millisecond. A frontend's logger hands each record here as well as showing it, the CLI's on
//! the console (`tmr_cli::setup_logging`), the window's in its errors list (`tmr_gui`'s logger), and both let through the records `level`
//! says, so every frontend writes the same file.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Log target for a record that goes to the log file only. The CLI's logger prints every other record on the console as well. Use it where
/// the console gets the same text another way, as WHEA events do through the ticker's queue.
pub const FILE_ONLY_TARGET: &str = "tmr::file_only";

/// The log file, once `open` has created it
static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// `RUST_LOG` as TMR reads it, the same for every frontend: its value (`info` without it), and the records a logger lets through, that
/// level and the ones above it. A value that isn't a level (a module's filter, as other programs take) is info.
pub fn level() -> (String, log::LevelFilter) {
    let log_level = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let level_filter = match log_level.to_lowercase().as_str() {
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => log::LevelFilter::Info,
    };
    (log_level, level_filter)
}

/// Creates `logs`, where the log file goes, if it isn't there.
pub fn create_dir() -> std::io::Result<()> {
    std::fs::create_dir_all("logs")
}

/// The log file's name. Shared with the result filename so a run's log and result can be paired by name; see `run_context::run_start`.
/// Local time — this file is read on the machine that wrote it.
pub fn path() -> String {
    format!("logs/{}.log", crate::run_context::run_file_stem())
}

/// Creates the log file at `path` with its header; `write` adds each record's line from then on. When it can't be created there is no log
/// file, and the records still go wherever the frontend shows them.
pub fn open(path: &str) {
    if let Ok(mut log_file) = std::fs::File::create(path) {
        // Stamp the zone once here rather than on every line: every line in the file has the same offset, so repeating it thousands of
        // times buys nothing, but a reader (or a later consolidation pass) still needs to know these are local times and which local that
        // was. The UTC equivalent is recorded so this file can be lined up with results from other boxes.
        let started = crate::run_context::run_start();
        let _ = writeln!(
            log_file,
            "# TMR {} — run started {} (UTC {}); all timestamps below are local time\n\
             # build: {}\n\
             # paired result file: results/{}.json",
            crate::BUILD.version,
            started.format("%Y-%m-%d %H:%M:%S %:z"),
            started.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S"),
            crate::BUILD.describe(),
            crate::run_context::run_file_stem(),
        );
        let _ = FILE.set(Mutex::new(log_file));
    }
}

/// Adds `record`'s line to the log file, without colours, when there is one.
pub fn write(record: &log::Record) {
    if let Some(file) = FILE.get()
        && let Ok(mut file) = file.lock()
    {
        let _ = writeln!(
            file,
            "[{} {} {}] {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.module_path().unwrap_or("unknown"),
            record.args()
        );
        let _ = file.flush();
    }
}
