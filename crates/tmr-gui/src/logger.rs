//! The window's logger (TODO 29 S6): every record into the log file through `tmr::logfile`, the
//! same lines as the CLI's, and the warnings and errors into the window's errors list as well, the
//! kernels' bad-word lines among them (`Rescan::bad`'s cold path), with no kernel change. A record
//! is handed over with `try_send` on a bounded channel, so a worker that logs never waits for the
//! window: in an error storm the channel fills, the lines it can't take are counted, and the
//! window says how many it dropped. The file has every line.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// The lines the channel holds: the window drains it every 500 ms
const QUEUED: usize = 4096;

/// The window's logger.
struct GuiLogger {
    /// The records the log file gets, from `RUST_LOG` as the CLI reads it
    level: LevelFilter,
    lines: SyncSender<String>,
    /// The lines the channel was full for
    dropped: Arc<AtomicU64>,
}

/// The window's side of the logger: the warnings and errors logged, and how many the channel
/// couldn't take.
pub(crate) struct LogLines {
    lines: Receiver<String>,
    dropped: Arc<AtomicU64>,
}

impl LogLines {
    /// The lines queued since the last call, and how many have been dropped so far
    pub(crate) fn take(&self) -> (Vec<String>, u64) {
        (self.lines.try_iter().collect(), self.dropped.load(Ordering::Relaxed))
    }
}

/// A logger letting `level` through, and its window's side, with room for `queued` lines
fn logger(level: LevelFilter, queued: usize) -> (GuiLogger, LogLines) {
    let (tx, rx) = mpsc::sync_channel(queued);
    let dropped = Arc::new(AtomicU64::new(0));
    (GuiLogger { level, lines: tx, dropped: Arc::clone(&dropped) }, LogLines { lines: rx, dropped })
}

impl Log for GuiLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        tmr::logfile::write(record);
        // As on the CLI's console, a record for the file only isn't shown: the window has its text
        // another way (WHEA's lines from the progress, a session's error from its end)
        if record.level() <= Level::Warn && record.target() != tmr::logfile::FILE_ONLY_TARGET {
            let line = format!(
                "[{} {} {}] {}",
                chrono::Local::now().format("%H:%M:%S"),
                record.level(),
                record.module_path().unwrap_or("unknown"),
                record.args()
            );
            // Disconnected only once the window has gone, and then there is nothing to show
            if let Err(TrySendError::Full(_)) = self.lines.try_send(line) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Each line is flushed as it is written (`tmr::logfile::write`)
    fn flush(&self) {}
}

/// Sets up the window's logging at Start, once a process (one session a process): the log file,
/// opened now so its name pairs with the run's results (`tmr::logfile::path`), then this logger,
/// and the two records the CLI's logging starts with. Returns the window's side.
pub(crate) fn install() -> LogLines {
    let made = tmr::logfile::create_dir();
    let (rust_log, level) = tmr::logfile::level();
    let path = tmr::logfile::path();
    tmr::logfile::open(&path);
    let (logger, lines) = logger(level, QUEUED);
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
    log::info!("TMR v{} started - warnings and errors also shown in the window", tmr::BUILD.version);
    log::info!("Log level: {} - detailed logs also saved to {}", rust_log, path);
    // In the errors list, where the CLI prints it on the console
    if let Err(e) = made {
        log::warn!("Failed to create logs directory: {}", e);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Warnings and errors reach the window, the file's records alone don't, and a full channel
    /// counts what it drops instead of waiting (TODO 29 S6)
    #[test]
    fn warnings_reach_the_window_and_a_full_channel_counts_what_it_drops() {
        let (logger, window) = logger(LevelFilter::Info, 2);
        let log = |level: Level, target: &str, text: &str| {
            logger.log(&Record::builder().level(level).target(target).module_path_static(Some("tmr::tests"))
                .args(format_args!("{text}")).build());
        };
        log(Level::Info, "tmr::tests", "a step began");
        log(Level::Debug, "tmr::tests", "below the level");
        log(Level::Warn, "tmr::tests", "bad word at 0x1000");
        log(Level::Error, tmr::logfile::FILE_ONLY_TARGET, "WHEA [corrected] id=47");
        log(Level::Error, "tmr::tests", "second bad word");
        log(Level::Error, "tmr::tests", "third bad word");
        // Each line without its stamp, `[hh:mm:ss `
        let take = || {
            let (lines, dropped) = window.take();
            assert!(lines.iter().all(|line| line.starts_with('[') && line.as_bytes()[9] == b' '), "{lines:?}");
            (lines.iter().map(|line| line[10..].to_string()).collect::<Vec<_>>(), dropped)
        };
        assert_eq!(take(), (vec!["WARN tmr::tests] bad word at 0x1000".to_string(), "ERROR tmr::tests] second bad word".to_string()], 1),
                   "the third dropped, with the channel full");

        // Drained, it takes lines again, and the count stays the run's
        log(Level::Warn, "tmr::tests", "fourth");
        assert_eq!(take(), (vec!["WARN tmr::tests] fourth".to_string()], 1));
    }
}
