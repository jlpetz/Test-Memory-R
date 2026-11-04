//! The window's logger (TODO 29 S6, TODO 109): every record into the log file through `tmr::logfile`, the same lines as the CLI's, and into
//! the window as well, for its Log tab, the warnings and errors for its errors list too, the kernels' bad-word lines among them
//! (`Rescan::bad`'s cold path), with no kernel change. The Log tab takes the console's text of each event too (`Transcript`), so it has the
//! run as the console shows it. A record is handed over with `try_send` on a bounded channel, so a worker that logs
//! never waits for the window: in an error storm the channel fills, the lines it can't take are counted, and the window says how many it
//! dropped. The file has every line.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// The lines the channel holds: the window drains it every 500 ms
const QUEUED: usize = 4096;

/// A line logged, as the log file has it: `[12:00:00 WARN tmr::tests] bad word at 0x1000`
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogLine {
    pub text: String,
    /// A warning or an error, which the errors list takes too. Not one for the file only: the window has that text another way (WHEA's
    /// lines from the progress, a session's error from its end), as the CLI's console does.
    pub alarm: bool,
}

/// The window's logger.
struct GuiLogger {
    /// The most it lets through (`Levels::most`)
    level: LevelFilter,
    /// What the log file takes and what the Log tab shows, from `RUST_LOG` as the CLI reads it
    levels: tmr::logfile::Levels,
    lines: SyncSender<LogLine>,
    /// The lines the channel was full for
    dropped: Arc<AtomicU64>,
}

/// The window's side of the logger: the lines logged, and how many the channel couldn't take.
pub(crate) struct LogLines {
    lines: Receiver<LogLine>,
    dropped: Arc<AtomicU64>,
    transcript: Transcript,
}

impl LogLines {
    /// The lines queued since the last call, and how many have been dropped so far
    pub(crate) fn take(&self) -> (Vec<LogLine>, u64) {
        (self.lines.try_iter().collect(), self.dropped.load(Ordering::Relaxed))
    }

    /// Where the console's text of each event goes, into the same channel as the records
    pub(crate) fn transcript(&self) -> Transcript {
        self.transcript.clone()
    }
}

/// The console's text of each event, for the Log tab (TODO 109): a line each, without its colours, into the log's channel, from the
/// engine's thread as the event comes, so it comes in the console's order with the records. A full channel counts what it drops, as the
/// logger does.
#[derive(Clone)]
pub(crate) struct Transcript {
    lines: SyncSender<LogLine>,
    dropped: Arc<AtomicU64>,
}

impl Transcript {
    pub(crate) fn send(&self, shown: &str) {
        for line in tmr::logfile::without_colours(shown).lines() {
            if let Err(TrySendError::Full(_)) = self.lines.try_send(LogLine { text: line.to_string(), alarm: false }) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// A logger taking what `levels` says, and its window's side, with room for `queued` lines
fn logger(levels: tmr::logfile::Levels, queued: usize) -> (GuiLogger, LogLines) {
    let (tx, rx) = mpsc::sync_channel(queued);
    let dropped = Arc::new(AtomicU64::new(0));
    let transcript = Transcript { lines: tx.clone(), dropped: Arc::clone(&dropped) };
    (GuiLogger { level: levels.most(), levels, lines: tx, dropped: Arc::clone(&dropped) }, LogLines { lines: rx, dropped, transcript })
}

impl Log for GuiLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if self.levels.to_file(record) {
            tmr::logfile::write(record);
        }
        let alarm = record.level() <= Level::Warn && record.target() != tmr::logfile::FILE_ONLY_TARGET;
        // The Log tab shows what the console would (TODO 105): the run's own lines as their time and text, the records at its level, a
        // warning in the errors list whatever the level
        let text = if record.target() == tmr::logfile::SHOWN_TARGET {
            tmr::logfile::shown_line(record)
        // The file-only records too (the WHEA lines, which the console shows through its ticker): the window has no ticker
        } else if record.level() <= self.levels.shown || alarm {
            format!(
                "[{} {} {}] {}",
                chrono::Local::now().format("%H:%M:%S"),
                record.level(),
                record.module_path().unwrap_or("unknown"),
                record.args()
            )
        } else {
            return;
        };
        // Disconnected only once the window has gone, and then there is nothing to show
        if let Err(TrySendError::Full(_)) = self.lines.try_send(LogLine { text, alarm }) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Each line is flushed as it is written (`tmr::logfile::write`)
    fn flush(&self) {}
}

/// The window's logger, at the first Start: once a process, as the `log` crate takes one logger. Returns the window's side, which the
/// window keeps from run to run.
pub(crate) fn install() -> LogLines {
    let levels = tmr::logfile::levels();
    let most = levels.most();
    let (logger, lines) = logger(levels, QUEUED);
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(most);
    }
    lines
}

/// A run's log file, as the run starts (TODO 109: a file a run, so a run after another has its own, its name pairing with its results;
/// `tmr::session::reset_run` first gives the run its stamp), and the two records the CLI's logging starts with. The window closes it as
/// the session ends (`tmr::logfile::close`).
pub(crate) fn open_log_file() {
    let made = tmr::logfile::create_dir();
    let levels = tmr::logfile::levels();
    let path = tmr::logfile::path();
    let opened = tmr::logfile::open(&path);
    log::info!(target: tmr::logfile::FILE_ONLY_TARGET, "TMR v{} started, in its window", tmr::BUILD.version);
    // In the errors list, where the CLI prints them on the console
    match (made, opened) {
        (Err(e), _) => log::warn!("Failed to create logs directory: {}", e),
        (Ok(()), Err(e)) => log::warn!("No log file, {}: {e}", path.display()),
        (Ok(()), Ok(())) => log::info!(target: tmr::logfile::SHOWN_TARGET, "📝 Log file: {} ({})", path.display(), levels.described()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every record the level lets through reaches the window, the warnings and errors as alarms but for the file's own, and a full channel
    /// counts what it drops instead of waiting (TODO 29 S6, TODO 109)
    #[test]
    fn the_log_reaches_the_window_and_a_full_channel_counts_what_it_drops() {
        let levels = tmr::logfile::Levels { rust_log: None, file: LevelFilter::Info, shown: LevelFilter::Info };
        let (logger, window) = logger(levels, 4);
        let log = |level: Level, target: &str, text: &str| {
            logger.log(
                &Record::builder().level(level).target(target).module_path_static(Some("tmr::tests")).args(format_args!("{text}")).build(),
            );
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
            assert!(lines.iter().all(|line| line.text.starts_with('[') && line.text.as_bytes()[9] == b' '), "{lines:?}");
            (lines.iter().map(|line| (line.text[10..].to_string(), line.alarm)).collect::<Vec<_>>(), dropped)
        };
        let line = |text: &str, alarm| (text.to_string(), alarm);
        assert_eq!(
            take(),
            (
                vec![
                    line("INFO tmr::tests] a step began", false),
                    line("WARN tmr::tests] bad word at 0x1000", true),
                    line("ERROR tmr::tests] WHEA [corrected] id=47", false),
                    line("ERROR tmr::tests] second bad word", true)
                ],
                1
            ),
            "the third dropped, with the channel full; the file's own line no alarm"
        );

        // Drained, it takes lines again, and the count stays the run's
        log(Level::Warn, "tmr::tests", "fourth");
        assert_eq!(take(), (vec![line("WARN tmr::tests] fourth", true)], 1));

        // The console's text of an event, a line each, without its colours, in the records' order
        window.transcript().send("\u{1b}[1m📊 Test Configuration\u{1b}[0m\n  1  Mem-StuckBit512_A\n\n");
        log(Level::Info, "tmr::tests", "after it");
        let (lines, _) = window.take();
        let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(texts[..3], ["📊 Test Configuration", "  1  Mem-StuckBit512_A", ""]);
        assert!(texts[3].ends_with("INFO tmr::tests] after it") && lines.iter().all(|line| !line.alarm), "{lines:?}");
    }
}
