//! The one path to the terminal while the progress ticker is on screen.
//!
//! The ticker sits under the last line of output and is redrawn in place, which means moving the
//! cursor back up over every row it covers. Anything another thread printed in the meantime would be
//! overwritten by that, or stranded between two ticker rows. So everything that can print while the
//! ticker is up — the logger, WHEA events, the Ctrl+C handler, the runner — goes through here: each
//! print takes the ticker down, writes, and puts it back under one lock, so the ticker is always the
//! last thing on screen and is never torn.
//!
//! Plain `println!` is still fine wherever no ticker can be drawn: before the suite starts, after it
//! finishes, and between `hold` and `release`.

// Every `unsafe` block in this file is a Win32 console call. See memory/backend.rs for why this
// lint is scoped per file rather than crate-wide.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard};

use windows::Win32::System::Console::{
    CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode,
};

struct Ticker {
    /// The ticker as last drawn, kept so it can be put back after a print. Empty when there is
    /// nothing to put back.
    text: String,
    /// Screen rows the ticker covers now, wrapped rows included. 0 = not on screen.
    rows: usize,
    /// A report is printing with plain `println!`: draw nothing until `release`.
    held: bool,
}

static TICKER: Mutex<Ticker> = Mutex::new(Ticker { text: String::new(), rows: 0, held: false });

/// Lock order is `TICKER`, then stdout. Plain `println!` takes only stdout, so it cannot deadlock
/// against this.
fn lock() -> (MutexGuard<'static, Ticker>, io::StdoutLock<'static>) {
    let ticker = TICKER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    (ticker, io::stdout().lock())
}

impl Ticker {
    /// Take the ticker off the screen, leaving the cursor where its first row began. `text` is
    /// kept for the redraw.
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

/// Replace the ticker with `text`, printing the lines from `above` first. Draws nothing, and does
/// not call `above`, while a report holds the console. Returns whether it drew.
pub fn draw_ticker(text: String, above: impl FnOnce() -> Vec<String>) -> bool {
    let (mut ticker, mut out) = lock();
    if ticker.held {
        return false;
    }
    ticker.erase(&mut out);
    for line in above() {
        let _ = writeln!(out, "{}", line);
    }
    ticker.draw(&mut out, text);
    let _ = out.flush();
    true
}

/// Take the ticker down and keep it down, so a multi-line report can print with plain `println!`.
/// Other writers can still print through `print_above` meanwhile; they just don't redraw it. At
/// the end of the suite it is never released.
pub fn hold() {
    let (mut ticker, mut out) = lock();
    ticker.erase(&mut out);
    ticker.text.clear();
    ticker.held = true;
    let _ = out.flush();
}

/// End a `hold`. The ticker comes back at its next scheduled draw, with fresh figures rather than
/// the ones from before the report.
pub fn release() {
    lock().0.held = false;
}

/// Log target for a record that goes to the log file only. The logger (`cli.rs`) prints every other
/// record here as well. Use it where the console gets the same text another way, as WHEA events do
/// through the ticker's queue.
pub const FILE_ONLY_TARGET: &str = "tmr::file_only";

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

/// Switch stdout to VT processing, which the ticker's cursor movement and the log colours both
/// need. Windows Terminal always has it on; classic conhost only for programs that ask. It used to
/// be switched on as a side effect of env_logger writing to stdout, which stopped when the logger
/// moved to `LogSink`, so it is done explicitly here. A no-op when stdout is not a console.
pub fn init() {
    // SAFETY: no pointer arguments. The handle is the process's own stdout, which we do not own
    // and never close.
    let Ok(handle) = (unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }) else {
        return;
    };
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: `mode` is a local passed directly as the out-parameter, so it outlives the call.
    // Fails, harmlessly, when stdout is redirected.
    if unsafe { GetConsoleMode(handle, &mut mode) }.is_ok() {
        // SAFETY: value arguments only, on the console handle `GetConsoleMode` just accepted.
        let _ = unsafe { SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) };
    }
}

/// The column the console wraps at, or `usize::MAX` when stdout is not a console, where nothing
/// wraps. Read on every draw, so a resized window is picked up at the next one.
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

/// Rows `text` covers on a console `width` columns wide. Everything the ticker prints is ASCII
/// except ⚠️ (U+26A0 plus the U+FE0F presentation selector), which terminals draw two columns wide,
/// so a non-ASCII char counts as two columns and the selector as none.
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
