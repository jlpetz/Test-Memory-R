//! The log file (TODO 29): `logs\<stem>.log` in TMR's home (`home.rs`), with the stem the run's results file shares, a header, then one
//! line per record, `[stamp LEVEL module] message`, in local time to the millisecond. A frontend's logger hands each record here as well as
//! showing it, the CLI's on the console (`tmr_cli::setup_logging`), the window's in its errors list (`tmr_gui`'s logger), and both let
//! through the records `level` says, so every frontend writes the same file.

use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Log target for a record that goes to the log file only. The CLI's logger prints every other record on the console as well. Use it where
/// the console gets the same text another way, as WHEA events do through the ticker's queue.
pub const FILE_ONLY_TARGET: &str = "tmr::file_only";

/// The run's log file, once `open` has created it, until `close` (TODO 109: a file a run, so a session after another has its own)
static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

fn file() -> MutexGuard<'static, Option<std::fs::File>> {
    FILE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The run's log file is done with: the records from here on go to no file, until the next run's opens. The window's, as a session ends
/// (TODO 109); the console's lasts as long as its process.
pub fn close() {
    *file() = None;
}

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

/// Whether a command line (the exe's name first) keeps a log file: a session does, and so do the one-shot commands that change the system
/// (`--setup-large-pages` and `--remove-large-pages`, the calibrations, which save to tmr-cfg.json). `--help`, `--version`, the topology
/// commands, `--compare-results`, `--show-results` and `--create-demo-configs` don't: a log of every `tmr --version` was a folder of files,
/// and winget's checks run it and fail a package that leaves files behind.
pub fn wanted(args: &[String]) -> bool {
    use crate::commands::Command;
    // Without the mode flags, as the session parses it (`session::MODE_FLAGS`): `tmr --quick-test --help` is `--help`
    let args: Vec<String> = args.iter().filter(|a| !crate::session::MODE_FLAGS.contains(&a.as_str())).cloned().collect();
    matches!(Command::parse(&args), None | Some(Command::SetupLargePages(_) | Command::RemoveLargePages(_) | Command::Calibrate(_)))
}

/// Creates `logs\` in TMR's home, where the log file goes, if it isn't there.
pub fn create_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(crate::home::logs())
}

/// The log file's path. Its name is shared with the result filename so a run's log and result can be paired by name, then by the run ID
/// both carry; see `run_context::run_file_stem`. Local time — this file is read on the machine that wrote it.
pub fn path() -> std::path::PathBuf {
    crate::home::logs().join(format!("{}.log", crate::run_context::run_file_stem()))
}

/// Creates the log file at `path` with its header; `write` adds each record's line from then on. When it can't be created there is no log
/// file, the records still go wherever the frontend shows them, and the error says why, for the frontend to show. Only a new file: in an
/// install for all users `logs\` is writable by every user, and TMR may run elevated, so it never opens a file someone else put there, a
/// link among them (TODO 104). A second run in the same second, whose name is taken, has no log file.
pub fn open(path: &std::path::Path) -> std::io::Result<()> {
    let mut log_file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    // Stamp the zone once here rather than on every line: every line in the file has the same offset, so repeating it thousands of
    // times buys nothing, but a reader (or a later consolidation pass) still needs to know these are local times and which local that
    // was. The UTC equivalent is recorded so this file can be lined up with results from other boxes.
    let started = crate::run_context::run_start();
    let _ = writeln!(
        log_file,
        "# TMR {} — run started {} (UTC {}); all timestamps below are local time\n\
         # build: {}\n\
         # paired result file: results/{}.json\n\
         {RUN_ID_LINE}{}",
        crate::BUILD.version,
        started.format("%Y-%m-%d %H:%M:%S %:z"),
        started.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S"),
        crate::BUILD.describe(),
        crate::run_context::run_file_stem(),
        crate::run_context::run_id(),
    );
    *file() = Some(log_file);
    Ok(())
}

/// The header's line with the run's ID (`run_context::run_id`), before the ID
const RUN_ID_LINE: &str = "# run id: ";

/// The run ID in the header of the log file at `path` (`open`'s `# run id:` line), to check against a results file's
/// (`metadata.run_id`) once their names pair them (`run_context::run_file_stem`). None for a log without one, as written before it.
pub fn run_id_in(path: &std::path::Path) -> std::io::Result<Option<String>> {
    use std::io::BufRead;
    for line in std::io::BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        // The header is the lines that start with `#`, the records after it
        if !line.starts_with('#') {
            break;
        }
        if let Some(id) = line.strip_prefix(RUN_ID_LINE) {
            return Ok(Some(id.to_string()));
        }
    }
    Ok(None)
}

/// Adds `text`, what a frontend showed between the records (TODO 29's transcript, TODO 109), to the log file as it is, without its colours,
/// when there is one: so the file has the run as the console had it, the window's runs too
pub fn write_text(text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(file) = file().as_mut() {
        let text = without_colours(text);
        let _ = file.write_all(text.as_bytes());
        if !text.ends_with('\n') {
            let _ = file.write_all(b"\n");
        }
        let _ = file.flush();
    }
}

/// `text` without its ANSI colour sequences (`ESC [` … a letter), which the console's renderer puts in its headings
pub fn without_colours(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC [, then parameters, then the final byte, @ to ~
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Adds `record`'s line to the log file, without colours, when there is one.
pub fn write(record: &log::Record) {
    if let Some(file) = file().as_mut() {
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

#[cfg(test)]
mod tests {
    /// The transcript leaves the console's colours out, and keeps the rest as it was, a line's emoji and its breaks
    #[test]
    fn the_transcript_has_no_colours() {
        let shown = "\u{1b}[1;36m📊 Final Test Summary\u{1b}[0m\n  plain, 50% [x]\n";
        assert_eq!(super::without_colours(shown), "📊 Final Test Summary\n  plain, 50% [x]\n");
        assert_eq!(super::without_colours("no colours"), "no colours");
    }

    /// A run's log file and its results file, as saved, carry the same run ID, 32 hex digits, the log's in its header and the results' in
    /// their metadata; the next run in the process has another, after `reset_run_stamp` (`session::reset_run`'s, for the window's Run
    /// again; the whole reset would change settings other tests in the process use). The owner: a renamed file could pair one run's log
    /// with another's results, so a tool checks the name, then the ID.
    #[test]
    fn a_run_s_log_and_results_carry_its_id_and_the_next_run_has_another() {
        use crate::results::{TempDir, TestRunResult};
        let dir = TempDir::new("run-id");
        // The run's log and results files, in the temp directory, and the ID each has
        let ids = |run: &str| {
            let log = std::path::PathBuf::from(dir.file(&format!("{run}.log")));
            super::open(&log).unwrap_or_else(|e| panic!("{}: {e}", log.display()));
            super::close();
            let results = dir.file(&format!("{run}.json"));
            TestRunResult::new(Default::default()).save_atomic(&results).unwrap_or_else(|e| panic!("{e}"));
            let logged = super::run_id_in(&log).unwrap().expect("the log's header has the run ID");
            let saved = TestRunResult::load_from_file(&results).unwrap().metadata.run_id.expect("the results have the run ID");
            (logged, saved)
        };
        let (logged, saved) = ids("first");
        assert_eq!(logged, saved);
        assert_eq!(logged, crate::run_context::run_id());
        assert!(logged.len() == 32 && logged.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')), "{logged}");
        crate::run_context::reset_run_stamp();
        let (next_logged, next_saved) = ids("next");
        assert_eq!(next_logged, next_saved);
        assert_ne!(next_logged, logged, "a new run, a new ID");
    }
}
