//! The console's output against goldens (TODO 29): what a test printed, captured, compared with this crate's `testdata/console/<name>.txt`.
//! The engine/presentation split moved every print behind a frontend, and into this crate, and these show the moved prints still write the
//! bytes they did. `TMR_BLESS=1 cargo test` writes the goldens instead of comparing them; `git diff crates/tmr-cli/testdata/` then shows
//! what changed. The engine's code a golden runs it reaches through `tmr::test_support`.
//!
//! The capture is the CLI's own: every write the CLI makes goes through the console's writer (`console::Writer`), which in a test build
//! hands it to the capture on now (`write`). The capture is the process's, not a thread's, so the workers' lines land in it too, and
//! `Logger` prints each log record as the CLI's logger does (`setup_logging`), through `console::print_above`, so the records and their
//! order against the prints are in the golden. Any other test's records would land in it as well, so the tests that capture or log take
//! turns at the console (`turn`): a capture takes one for itself, and a test that logs outside its captures (one that builds the engine's
//! state first, a fake cycle's) holds one through its whole body. A capture sets the console flag, on as on a console (`capture`, as every
//! golden has it) or off as redirected (`capture_redirected`), so the goldens of text that read the flag without printing it (the
//! Reporter's, `renderer/tests.rs`) render in a capture too.
//!
//! The capture takes stdout and stderr together, so which of the two a line goes to isn't in a golden: the owner's A/B of the exe captures
//! them apart (`tools/console_diff.py`).

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::console::Stream;

/// What the CLI wrote while a capture is on, both streams': `None` when none is
static SINK: Mutex<Option<Vec<u8>>> = Mutex::new(None);

fn sink() -> MutexGuard<'static, Option<Vec<u8>>> {
    SINK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where a test build's console writes go (`console::Writer`): into the capture on now; with none on (what a test logs outside its
/// captures), to libtest's capture of this thread, as a test's own prints go, so they show only when the test fails or under `--nocapture`.
pub(crate) fn write(stream: Stream, bytes: &[u8]) {
    let mut sink = sink();
    if let Some(sink) = sink.as_mut() {
        sink.extend_from_slice(bytes);
        return;
    }
    // Every write is of whole `str`s, so none splits a char
    let text = String::from_utf8_lossy(bytes);
    match stream {
        Stream::Out => print!("{text}"),
        Stream::Err => eprint!("{text}"),
    }
}

/// One test at the console at a time: the sink is the process's, and so is the console flag a capture sets
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

thread_local! {
    /// This thread's turn at the console while it has one, and how many `Turn`s hold it
    static TURN: RefCell<(Option<MutexGuard<'static, ()>>, usize)> = const { RefCell::new((None, 0)) };
}

/// A test's turn at the console, for as long as it lives (`turn`). Not `Send`: it ends on the thread that took it.
pub(crate) struct Turn(PhantomData<*const ()>);

/// Waits for the console, and has it till the `Turn` drops: no other test's capture is on meanwhile, nor its records. A thread that has a
/// turn can take another, a capture inside a test's turn its own.
pub(crate) fn turn() -> Turn {
    TURN.with_borrow_mut(|(guard, held)| {
        if guard.is_none() {
            *guard = Some(ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner));
        }
        *held += 1;
    });
    Turn(PhantomData)
}

impl Drop for Turn {
    fn drop(&mut self) {
        TURN.with_borrow_mut(|(guard, held)| {
            *held -= 1;
            if *held == 0 {
                *guard = None;
            }
        });
    }
}

/// Runs `f` and returns what it printed, as on a console, with its threads' prints and every log record from Info up. Not nested.
pub(crate) fn capture<R>(f: impl FnOnce() -> R) -> (R, String) {
    capture_as(true, f)
}

/// `capture`, as on a stdout that isn't a console: no escape codes and no ticker (`console::is_console`).
pub(crate) fn capture_redirected<R>(f: impl FnOnce() -> R) -> (R, String) {
    capture_as(false, f)
}

/// `capture`, stdout a console or not
fn capture_as<R>(console: bool, f: impl FnOnce() -> R) -> (R, String) {
    let _turn = turn();
    install_logger();
    // The detections log what they found at their first use, wherever that is: into a capture of their own, dropped, so no golden depends
    // on which test ran first
    *sink() = Some(Vec::new());
    tmr::test_support::detect();
    crate::console::set_console(console);
    *sink() = Some(Vec::new());
    let _off = Off;
    let value = f();
    let bytes = sink().take().unwrap_or_default();
    (value, String::from_utf8(bytes).expect("the console writes UTF-8"))
}

thread_local!(static DEBUG_RECORDS: Cell<bool> = const { Cell::new(false) });

/// `capture`, with this thread's Debug records too, as `RUST_LOG=debug` shows them: where an event lands against the records only that
/// level has. Other threads' records stay from Info up.
pub(crate) fn capture_with_debug<R>(f: impl FnOnce() -> R) -> (R, String) {
    /// Ends the Debug records however `f` ends
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            DEBUG_RECORDS.set(false);
        }
    }
    DEBUG_RECORDS.set(true);
    let _stop = Stop;
    capture(f)
}

/// Turns the capture off however `capture`'s closure ends. On a panic, what the closure printed goes to libtest's capture of this thread,
/// so the failure shows it.
struct Off;

impl Drop for Off {
    fn drop(&mut self) {
        let printed = sink().take();
        if let Some(printed) = printed.filter(|_| std::thread::panicking()) {
            print!("{}", String::from_utf8_lossy(&printed));
        }
    }
}

/// Compares `text` with the golden `testdata/console/<name>.txt` of this crate, or under
/// `TMR_BLESS=1` writes it there, as the engine's goldens are (`tmr::test_support::check_golden`).
pub(crate) fn check(name: &str, text: &str) {
    tmr::test_support::check_golden(&Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("console"), name, text);
}

/// `text` with the log records' stamps masked, the one figure in them that changes run to run.
pub(crate) fn mask_stamps(text: &str) -> String {
    mask_shape(text, "9999-99-99 99:99:99", "<stamp>")
}

/// The plan's output with what depends on this machine masked: the log stamps, and the size a chunk sized by a cache asks for (its row
/// names the cache, `(DRAM*2)`), which a note quotes when the thread's memory made the chunk smaller.
pub(crate) fn mask_plan(text: &str) -> String {
    let text = mask_stamps(text);
    // The tests whose chunk a cache sizes: their rows' second cell is the name
    let sized_by_cache: Vec<&str> = text
        .lines()
        .filter(|l| ["(L1", "(L2", "(L3", "(DRAM"].iter().any(|cache| l.contains(cache)))
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    text.split('\n')
        .map(|line| {
            let note = sized_by_cache.iter().find_map(|name| line.find(&format!("({name}) from ")).map(|at| at + name.len() + 8));
            match note.and_then(|from| line[from..].find(" -> ").map(|to| (from, from + to))) {
                Some((from, to)) => format!("{}<cache-sized>{}", &line[..from], &line[to..]),
                None => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A run's output with what changes from one run to the next masked: the log stamps, hh:mm:ss, what was measured (times, rates, latencies,
/// ratios and deviations: `mask_figures`, and a latency table's figures) and addresses. Labels, counts (ops, samples, cycles), byte counts,
/// glyphs and the order of the lines stay exact, but for two things the scheduler orders:
/// - the workers' lines: a run of lines that name a thread (`[Thread 1] Running ...`, `..., thread 0: idx 77`) is put in thread order, each
///   thread's own lines in theirs;
/// - the rows of a table of threads, sorted: the step's thread table orders its threads by their times, and its latency table by when each
///   finished. A table's columns are as wide as its widest cell, which a masked figure no longer shows, so its cells are joined by ` | `
///   and its rules cut to `-`.
pub(crate) fn mask_run(text: &str) -> String {
    let text = mask_shape(&mask_stamps(text), "99:99:99", "<hms>");
    let mut lines: Vec<String> = text.split('\n').map(mask_figures).collect();
    let mut at = 0;
    while at < lines.len() {
        let run = lines[at..].iter().take_while(|l| thread_of(l).is_some()).count();
        lines[at..at + run].sort_by_key(|l| thread_of(l));
        at += run.max(1);
    }
    let rule = |line: &str| line.len() >= 10 && line.bytes().all(|b| b == b'-');
    let cells = |line: &str| line.split("  ").map(str::trim).filter(|c| !c.is_empty()).map(str::to_string).collect::<Vec<_>>();
    // A bare decimal, `72.5`
    let decimal = |cell: &str| {
        cell.split_once('.').is_some_and(|(whole, part)| {
            !whole.is_empty() && !part.is_empty() && whole.bytes().chain(part.bytes()).all(|b| b.is_ascii_digit())
        })
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        // `TableBuilder::write_to`: a rule, the headers, a rule, the rows, a rule
        if rule(&lines[i]) && lines.get(i + 2).is_some_and(|l| rule(l)) {
            let end = (i + 3..lines.len()).find(|&j| rule(&lines[j])).unwrap_or(lines.len());
            let headers = cells(&lines[i + 1]);
            // A latency table's figures have no unit to know them by: each of its decimals is one
            let latency = headers.iter().any(|h| h == "P50");
            let mut rows: Vec<String> = lines[i + 3..end]
                .iter()
                .map(|l| {
                    cells(l).into_iter().map(|c| if latency && decimal(&c) { "#".to_string() } else { c }).collect::<Vec<_>>().join(" | ")
                })
                .collect();
            if headers.first().is_some_and(|h| h == "Thread") {
                rows.sort();
            }
            out.extend(["-".to_string(), headers.join(" | "), "-".to_string()]);
            out.extend(rows);
            out.push("-".to_string());
            i = end + 1;
        } else {
            out.push(lines[i].clone());
            i += 1;
        }
    }
    out.join("\n")
}

/// The thread a line names first, `Thread 3` or `thread 3`.
fn thread_of(line: &str) -> Option<usize> {
    line.match_indices("hread ").find_map(|(at, word)| {
        let named = at > 0 && matches!(line.as_bytes()[at - 1], b'T' | b't');
        let digits: String = line[at + word.len()..].chars().take_while(char::is_ascii_digit).collect();
        named.then(|| digits.parse().ok()).flatten()
    })
}

/// `text` with every run of `shape` (`9` a digit, any other char itself) replaced by `with`.
fn mask_shape(text: &str, shape: &str, with: &str) -> String {
    let (text, shape): (Vec<char>, Vec<char>) = (text.chars().collect(), shape.chars().collect());
    let fits = |at: usize| {
        text.len() - at >= shape.len() && shape.iter().zip(&text[at..]).all(|(&s, &c)| if s == '9' { c.is_ascii_digit() } else { s == c })
    };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if fits(i) {
            out.push_str(with);
            i += shape.len();
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    out
}

/// One line with what a run measured masked as `#`: a time (`0.4s`, `75.3ns`), a rate (`MiB/s`, `ops/s`), a ratio (`1.20x`), a deviation
/// (`+12.3`, signed) and a percentile (`P50=72.3`). A count stays (`163.84K ops`, `1 samples`, the Avg row's `1.0` cycles), as do byte
/// counts, whole numbers and hex.
fn mask_figures(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let rest = |at: usize| chars[at..].iter().collect::<String>();
    // A unit right after a figure, not running on into a word or a `/s`
    let unit = |at: usize, unit: &str| {
        let rest = rest(at);
        rest.strip_prefix(unit).is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '/'))
    };
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let in_word = i > 0 && (chars[i - 1].is_alphanumeric() || matches!(chars[i - 1], '_' | '.'));
        let signed = matches!(c, '+' | '-') && chars.get(i + 1).is_some_and(char::is_ascii_digit);
        if in_word || !(c.is_ascii_digit() || signed) {
            if c == 'a' && rest(i).starts_with("address 0x") {
                out.push_str("address <addr>");
                i += "address 0x".len();
                while chars.get(i).is_some_and(char::is_ascii_hexdigit) {
                    i += 1;
                }
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while chars.get(end).is_some_and(|&d| d.is_ascii_digit() || (d == '.' && chars.get(end + 1).is_some_and(char::is_ascii_digit))) {
            end += 1;
        }
        // An operation count's `K`, `M` or `B` (`1.23M ops/s`)
        if chars.get(end).is_some_and(|d| matches!(d, 'K' | 'M' | 'B')) && rest(end + 1).starts_with(" ops") {
            end += 1;
        }
        let decimal = chars[i..end].contains(&'.');
        let bytes = [" GiB", " MiB", " KiB", " B"].iter().any(|u| unit(end, u));
        let rate = [" MiB/s", " ops/s", "ns", "ms"].iter().any(|u| rest(end).starts_with(u));
        // Seconds and ratios as the reports print them (`{:.1}s`, `{:.2}x`)
        let measured = decimal && !bytes && (unit(end, "s") || unit(end, "x") || signed || (i > 0 && chars[i - 1] == '='));
        if rate || measured {
            out.push('#');
        } else {
            out.extend(&chars[i..end]);
        }
        i = end;
    }
    out
}

/// Prints each record from Info up, and Debug on a thread under `capture_with_debug`, as the CLI's logger prints it (`setup_logging`), in
/// colour on a console and without on a redirected stdout, through `print_above` as its `LogSink` does. File-only records print nothing,
/// as there.
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info || (metadata.level() == log::Level::Debug && DEBUG_RECORDS.get())
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) || record.target() == tmr::logfile::FILE_ONLY_TARGET {
            return;
        }
        let colour = match record.level() {
            log::Level::Error => "31",
            log::Level::Warn => "33",
            log::Level::Info => "32",
            log::Level::Debug => "36",
            log::Level::Trace => "35",
        };
        // As env_logger strips the escapes when stdout isn't a console (`WriteStyle::Never`)
        let (colour, reset) = if crate::console::is_console() { (format!("\x1b[{colour}m"), "\x1b[0m") } else { (String::new(), "") };
        // Each line of a record its prefix, as the CLI's logger gives it (`tmr::logfile::prefixed`): a rescan's lines are one record
        let prefix = format!(
            "{}[{} {} {}]{}",
            colour,
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            record.level(),
            record.module_path().unwrap_or("unknown"),
            reset
        );
        crate::console::print_above(&tmr::logfile::prefixed(&prefix, &record.args().to_string()));
    }

    fn flush(&self) {}
}

/// Installs `Logger` once a process: the first capture does, and every test's records print from then on, into the capture on then.
fn install_logger() {
    static LOGGER: Logger = Logger;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        log::set_logger(&LOGGER).expect("no other logger in a test build");
        log::set_max_level(log::LevelFilter::Debug);
    });
}

mod tests {
    use super::*;

    /// The capture has what the console module prints, on stdout and stderr and above the ticker, the log records, and what a spawned
    /// thread prints, in the order they were printed.
    #[test]
    fn the_capture_has_the_consoles_lines_the_logs_and_the_threads() {
        let ((), text) = capture(|| {
            crate::console::print("plain\n");
            crate::console::eprint("to stderr\n");
            crate::console::print_above("above");
            log::info!("a record");
            log::debug!("below the level");
            log::info!(target: tmr::logfile::FILE_ONLY_TARGET, "the log file's alone");
            std::thread::spawn(|| log::warn!("from a thread")).join().unwrap();
        });
        let text = mask_stamps(&text);
        assert_eq!(
            text,
            "plain\nto stderr\nabove\n\x1b[32m[<stamp> INFO tmr_cli::golden::tests]\x1b[0m a record\n\
                          \x1b[33m[<stamp> WARN tmr_cli::golden::tests]\x1b[0m from a thread\n"
        );
    }

    /// `mask_run` masks the figures that change between runs and keeps the rest.
    #[test]
    fn masks_keep_labels_counts_and_byte_counts() {
        assert_eq!(mask_figures("✅ 0.4s, 3 errors (bit 17) + 0 seal, Cycles 🟢 1/1"), "✅ #s, 3 errors (bit 17) + 0 seal, Cycles 🟢 1/1");
        assert_eq!(
            mask_figures("   Test: 1.2s, 0.00 GiB @ 1234 MiB/s, 163.84K ops @ 1.23M ops/s, P50: 75.3ns, Spread: 1.20x"),
            "   Test: #s, 0.00 GiB @ # MiB/s, 163.84K ops @ # ops/s, P50: #ns, Spread: #x"
        );
        assert_eq!(
            mask_figures("Mem-StuckBit256_A, Bench-Verify-TMR-10, +0.0s -1.5s +12.3, 640.00 KiB +0.00 B, 1.0, 512, extent=64.00 KiB"),
            "Mem-StuckBit256_A, Bench-Verify-TMR-10, #s #s #, 640.00 KiB +0.00 B, 1.0, 512, extent=64.00 KiB"
        );
        assert_eq!(
            mask_figures("[Thread 0] ReadLatency - 1000 samples | P1=72.3 P99.9=112.5 | Spread=1.08x"),
            "[Thread 0] ReadLatency - 1000 samples | P1=# P99.9=# | Spread=#x"
        );
        assert_eq!(
            mask_figures("idx 8709 (address 0x1f2e3d4c5b60): expected 0x5555, got 0x5557"),
            "idx 8709 (address <addr>): expected 0x5555, got 0x5557"
        );
        let table = "------------\n    Thread  Time  Cycles  \n------------\n         1  0.1s  1  \n         0  12.0s  1  \n       Avg  6.0s  1.0  \n------------\nafter";
        assert_eq!(mask_run(table), "-\nThread | Time | Cycles\n-\n0 | #s | 1\n1 | #s | 1\nAvg | #s | 1.0\n-\nafter");
        let latency = "------------\n    Thread  Samples   P50  Spread  \n------------\n         1     1000  72.5   1.08x  \n------------";
        assert_eq!(mask_run(latency), "-\nThread | Samples | P50 | Spread\n-\n1 | 1000 | # | #x\n-");
        // Only a table of threads is in an order its rows don't keep
        let plan = "------------\n    #  Test  \n------------\n    2  Mem-B  \n    1  Mem-A  \n------------";
        assert_eq!(mask_run(plan), "-\n# | Test\n-\n2 | Mem-B\n1 | Mem-A\n-");
        let workers = "before\n[Thread 1] Running x\n[Thread 0] Running x\nstep 2, thread 0: idx 7\nThread 0 reported 1 errors\nafter";
        assert_eq!(
            mask_run(workers),
            "before\n[Thread 0] Running x\nstep 2, thread 0: idx 7\nThread 0 reported 1 errors\n[Thread 1] Running x\nafter"
        );
        assert_eq!(mask_run("[2026-10-22 19:30:01 INFO tmr] Runtime: 00:00:03"), "[<stamp> INFO tmr] Runtime: <hms>");
    }
}
