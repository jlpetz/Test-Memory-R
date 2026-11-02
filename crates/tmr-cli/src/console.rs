//! The one path to the terminal: every write the CLI makes to stdout or stderr goes through this module's writer (`Writer`, and `print`
//! and `eprint` for a whole text), so a unit test build can take them all into a golden's capture (`golden.rs`). And whether stdout is a
//! console (`is_console`), the process's one answer to it: a redirected stdout gets no escape codes and no ticker.
//!
//! The ticker sits under the last line of output and is redrawn in place, which means moving the cursor back up over every row it covers.
//! Anything another thread printed in the meantime would be overwritten by that, or stranded between two ticker rows. So everything that
//! can print while the ticker is up — the logger, WHEA events, the Ctrl+C handler, the run's events (`render`) — goes through the ticker's
//! lock here: each print takes the ticker down, writes, and puts it back under one lock, so the ticker is always the last thing on screen
//! and is never torn.
//!
//! A plain `print` is still fine wherever no ticker can be drawn: before the suite starts, and after it finishes (`hold`). A test's report,
//! many lines long, prints in one piece through `print_block`.

// Every `unsafe` block in this file is a Win32 console call. The crate's lint (lib.rs) covers its shipped code; this one the tests too.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use windows::Win32::System::Console::{
    CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle,
    STD_OUTPUT_HANDLE, SetConsoleMode,
};

/// Whether stdout is a console: one flag for the process, set at startup (`init`) and read by everything that writes escape codes (the
/// log's colours, `setup_logging`; the reports' colours and bold, `ConsoleRenderer`) and by the ticker, which a redirected stdout doesn't
/// get at all (`draw_ticker`). Off until `init`, so a process that never calls it, the window's, renders its text without escapes.
static IS_CONSOLE: AtomicBool = AtomicBool::new(false);

/// Whether stdout is a console (`IS_CONSOLE`). Relaxed: a plain load on x86, and nothing else is ordered by it.
pub fn is_console() -> bool {
    IS_CONSOLE.load(Ordering::Relaxed)
}

/// Sets the flag `init` sets, for a golden's capture (`golden.rs`): on, as on a console, or off, as redirected
#[cfg(test)]
pub(crate) fn set_console(console: bool) {
    IS_CONSOLE.store(console, Ordering::Relaxed);
}

/// The CLI's two streams
#[derive(Clone, Copy)]
pub(crate) enum Stream {
    Out,
    Err,
}

/// Where the CLI writes, stdout or stderr: the one place it does. Each call takes the stream's lock for itself, as `io::Stdout`'s do: a
/// `write_all` (a whole `print`) or a `writeln!` lands in one piece, and a writer held for a long call (a one-shot command's) never blocks
/// another thread's. In a unit test build every write goes to the tests instead (`golden::write`): into a golden's capture while one is
/// on, both streams' into one text.
pub(crate) struct Writer(Stream);

/// The writer on stdout
pub(crate) fn stdout() -> Writer {
    Writer(Stream::Out)
}

/// The writer on stderr
pub(crate) fn stderr() -> Writer {
    Writer(Stream::Err)
}

impl Write for Writer {
    #[cfg(not(test))]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0 {
            Stream::Out => io::stdout().write(buf),
            Stream::Err => io::stderr().write(buf),
        }
    }

    // `io::Stdout`'s own, the whole text under one lock: the default loops on `write`, a lock each, and a console takes a long text in
    // several
    #[cfg(not(test))]
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self.0 {
            Stream::Out => io::stdout().write_all(buf),
            Stream::Err => io::stderr().write_all(buf),
        }
    }

    // As `write_all`, the whole line under one lock: the default writes each piece of the format alone
    #[cfg(not(test))]
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        match self.0 {
            Stream::Out => io::stdout().write_fmt(args),
            Stream::Err => io::stderr().write_fmt(args),
        }
    }

    #[cfg(test)]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        crate::golden::write(self.0, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0 {
            Stream::Out => io::stdout().flush(),
            Stream::Err => io::stderr().flush(),
        }
    }
}

/// Prints `text` on stdout as it is, where no ticker can be up (before the suite and after it, `hold`). A console that can't be written to
/// loses it, as the ticker's own writes do.
pub(crate) fn print(text: &str) {
    write_all(stdout(), text);
}

/// Prints `text` on stderr as it is, `print`'s other stream
pub(crate) fn eprint(text: &str) {
    write_all(stderr(), text);
}

fn write_all(mut out: Writer, text: &str) {
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

struct Ticker {
    /// The ticker as last drawn, kept so it can be put back after a print. Empty when there is nothing to put back.
    text: String,
    /// Screen rows the ticker covers now, wrapped rows included. 0 = not on screen.
    rows: usize,
    /// The suite is over (`hold`): draw nothing more.
    held: bool,
}

static TICKER: Mutex<Ticker> = Mutex::new(Ticker { text: String::new(), rows: 0, held: false });

/// Lock order is `TICKER`, then stdout, a write at a time (`Writer`): the ticker's lock keeps a print under it whole, as every write while
/// the ticker can be up takes it. A plain `print` takes only stdout, so it cannot deadlock against this.
fn lock() -> (MutexGuard<'static, Ticker>, Writer) {
    let ticker = TICKER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    (ticker, stdout())
}

impl Ticker {
    /// Take the ticker off the screen, leaving the cursor where its first row began. `text` is kept for the redraw.
    fn erase(&mut self, out: &mut impl Write) {
        // `ESC[0A` moves up one row, not zero, so a one-row ticker must not send it.
        let _ = match self.rows {
            0 => Ok(()),
            1 => write!(out, "\r\x1b[J"),
            rows => write!(out, "\r\x1b[{}A\x1b[J", rows - 1),
        };
        self.rows = 0;
    }

    /// Draw `text` where the cursor is, with no trailing newline so the next draw can replace it.
    fn draw(&mut self, out: &mut impl Write, text: String) {
        let _ = out.write_all(text.as_bytes());
        self.rows = screen_rows(&text, console_width());
        self.text = text;
    }
}

/// Print `text` above the ticker. It is printed as whole lines: a missing final newline is added.
pub fn print_above(text: &str) {
    let (mut ticker, mut out) = lock();
    ticker.erase(&mut out);
    let _ = out.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = out.write_all(b"\n");
    }
    let text = std::mem::take(&mut ticker.text);
    if !text.is_empty() {
        ticker.draw(&mut out, text);
    }
    let _ = out.flush();
}

/// Replace the ticker with `text`, printing the lines from `above` first. On a stdout that isn't a console, no ticker: only the lines from
/// `above`, so a redirected capture holds no ticker frames. Once the console is held it writes nothing, calls neither, and returns false;
/// otherwise true, a draw done.
pub fn draw_ticker(text: impl FnOnce() -> String, above: impl FnOnce() -> Vec<String>) -> bool {
    let (mut ticker, mut out) = lock();
    if ticker.held {
        return false;
    }
    ticker.erase(&mut out);
    for line in above() {
        let _ = writeln!(out, "{}", line);
    }
    if is_console() {
        ticker.draw(&mut out, text());
    }
    let _ = out.flush();
    true
}

/// Take the ticker down for good, at the end of the suite, so what follows can print with a plain `print`. Other writers can still print
/// through `print_above`; they just don't redraw it.
pub fn hold() {
    let (mut ticker, mut out) = lock();
    ticker.erase(&mut out);
    ticker.text.clear();
    ticker.held = true;
    let _ = out.flush();
}

/// Print a multi-line report in place of the ticker (TODO 29): the ticker comes down and the report is written under one lock, so no other
/// line can land inside it. The ticker comes back at its next scheduled draw, with fresh figures rather than the ones from before the
/// report.
pub fn print_block(text: &str) {
    let (mut ticker, mut out) = lock();
    ticker.erase(&mut out);
    ticker.text.clear();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// The logger's output: every write is one formatted record, printed above the ticker.
pub struct LogSink;

impl Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !buf.is_empty() {
            print_above(&String::from_utf8_lossy(buf));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Whether stdout is a console, into the process's flag (`is_console`), once, at startup (`setup_logging`): std's `is_terminal`, a console
/// or an MSYS terminal's pipe, as the logger's colours have always been decided. Then a console is switched to VT processing, which the
/// ticker's cursor movement and the log colours both need. Windows Terminal always has it on; classic conhost only for programs that ask.
/// It used to be switched on as a side effect of env_logger writing to stdout, which stopped when the logger moved to `LogSink`, so it is
/// done explicitly here.
pub fn init() {
    IS_CONSOLE.store(io::IsTerminal::is_terminal(&io::stdout()), Ordering::Relaxed);
    // SAFETY: no pointer arguments. The handle is the process's own stdout, which we do not own and never close.
    let Ok(handle) = (unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }) else {
        return;
    };
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: `mode` is a local passed directly as the out-parameter, so it outlives the call. Fails, harmlessly, when stdout is
    // redirected.
    if unsafe { GetConsoleMode(handle, &mut mode) }.is_ok() {
        // SAFETY: value arguments only, on the console handle `GetConsoleMode` just accepted.
        let _ = unsafe { SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) };
    }
}

/// The column the console wraps at, or `usize::MAX` when stdout is not a console, where nothing wraps. Read on every draw, so a resized
/// window is picked up at the next one.
fn console_width() -> usize {
    // SAFETY: as in `init`.
    let Ok(handle) = (unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }) else {
        return usize::MAX;
    };
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: `info` is a local passed directly as the out-parameter, so it outlives the call.
    match unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } {
        Ok(()) if info.dwSize.X > 0 => info.dwSize.X as usize,
        _ => usize::MAX,
    }
}

/// Rows `text` covers on a console `width` columns wide. Everything the ticker prints is ASCII except ⚠️ (U+26A0 plus the U+FE0F
/// presentation selector), which terminals draw two columns wide, so a non-ASCII char counts as two columns and the selector as none.
fn screen_rows(text: &str, width: usize) -> usize {
    text.split('\n')
        .map(|line| {
            let columns: usize = line
                .chars()
                .map(|c| match c {
                    '\u{FE0F}' => 0,
                    c if c.is_ascii() => 1,
                    _ => 2,
                })
                .sum();
            columns.div_ceil(width).max(1)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::screen_rows;

    #[test]
    fn rows_count_wraps_and_wide_glyphs() {
        assert_eq!(screen_rows("abc", 120), 1);
        assert_eq!(screen_rows("", 120), 1);
        assert_eq!(screen_rows("abc\ndef", 120), 2);
        // The ticker's leading blank line is a row of its own.
        assert_eq!(screen_rows("\nabc\ndef", 120), 3);
        // Exactly the width stays on one row; one more column wraps.
        assert_eq!(screen_rows(&"x".repeat(120), 120), 1);
        assert_eq!(screen_rows(&"x".repeat(121), 120), 2);
        // ⚠️ is two columns: 118 + 2 fills the row, 119 + 2 wraps.
        assert_eq!(screen_rows(&format!("{}⚠️", "x".repeat(118)), 120), 1);
        assert_eq!(screen_rows(&format!("{}⚠️", "x".repeat(119)), 120), 2);
        // Not a console: nothing wraps.
        assert_eq!(screen_rows(&"x".repeat(500), usize::MAX), 1);
    }
}
