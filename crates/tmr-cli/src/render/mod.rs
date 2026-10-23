//! The CLI's frontend (TODO 29): a session's events as the console has always shown them, its
//! setup's (`setup.rs`) and its run's (`run.rs`). Each event's text is the print group it replaced,
//! with the same format strings, rendered into a String and written with that group's primitive
//! before `event` returns, on the engine's thread: plain `print!` before the suite and after it,
//! `console::print_above` for a line while the ticker can be up, `console::print_block` for a
//! step's report. So the console keeps its bytes, and its order against the log records
//! (`tmr::frontend`). The large-page restart is asked on the console (`prompts.rs`).

mod run;
#[cfg(test)]
mod run_tests;
mod setup;
#[cfg(test)]
mod tests;
mod ticker;

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) use setup::show_end;
use tmr::frontend::{Event, Frontend, ProgressTracker, Reporter, RestartChoice};

use crate::renderer::{ConsoleRenderer, create_console_reporter};

/// The console's frontend: the run's events as text, and the ticker.
#[derive(Default)]
pub struct CliFrontend {
    /// When the ticker was last drawn: from the first tick, then at each draw
    last_draw: Mutex<Option<Instant>>,
}

impl CliFrontend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Frontend for CliFrontend {
    fn event(&self, event: Event) {
        // No wildcard arm: an event the CLI doesn't show fails to compile
        match event {
            Event::Setup(setup) => setup::show(setup, crate::take_ctrl_c),
            Event::Run(run) => run::show(run),
        }
    }

    /// Asks on the console. A console that can't be written to is no answer, as the input's end is.
    fn large_page_restart(&self) -> RestartChoice {
        crate::prompts::large_page_restart(&mut std::io::stdin().lock(), &mut std::io::stdout()).unwrap_or(RestartChoice::Continue4K)
    }

    /// The ticker, redrawn every 2 seconds. Anything WHEA reported since the last draw is printed
    /// above it.
    fn tick(&self, progress: &ProgressTracker) {
        let mut last_draw = self.last_draw.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        draw_when_due(&mut last_draw, Instant::now(), || {
            // `None` until the suite has begun.
            let Some(snapshot) = progress.snapshot() else {
                return false;
            };
            // The WHEA lines name the failing component, which is what tells a marginal DIMM from
            // a marginal NIC. Once the console is held this draws nothing and leaves them queued.
            crate::console::draw_ticker(ticker::render(&snapshot), || progress.take_whea_descriptions())
        });
    }

    fn ticks_done(&self, progress: &ProgressTracker) {
        // Take the ticker down for good. Everything from here on prints with plain `print!`.
        crate::console::hold();

        // Events the OS queued since the last draw (the monitor polled on this tick), not printed
        // yet. Events already shown live are not reprinted: they are in the log file. No run tally
        // here either: the final summary's `Total WHEA` row prints it a screen later, and saying it
        // twice is noise. The runner flushes anything later still, once monitoring stops.
        let text: String = progress.take_whea_descriptions().iter().map(|event| format!("{event}\n")).collect();
        print!("{text}");
    }
}

/// Calls `draw` at `now` if the ticker is due: 2 seconds after its last draw, the first tick
/// starting the clock. `last_draw` moves only when `draw` drew (returns true), so a draw that
/// couldn't happen is tried again at the next tick.
fn draw_when_due(last_draw: &mut Option<Instant>, now: Instant, draw: impl FnOnce() -> bool) {
    if now.saturating_duration_since(*last_draw.get_or_insert(now)) < Duration::from_secs(2) {
        return;
    }
    if draw() {
        *last_draw = Some(now);
    }
}

/// A report as the console shows it: what the console reporter rendered, and whether it could.
pub(crate) fn rendered(report: impl FnOnce(&mut Reporter<ConsoleRenderer>) -> crate::Result<()>) -> (crate::Result<()>, String) {
    let mut reporter = create_console_reporter();
    let result = report(&mut reporter);
    (result, reporter.into_renderer().into_text())
}

/// `rendered`'s text, a report that fails to render logged as `what`, as the runner did.
fn report_text(what: &str, report: impl FnOnce(&mut Reporter<ConsoleRenderer>) -> crate::Result<()>) -> String {
    let (result, text) = rendered(report);
    if let Err(e) = result {
        log::error!("Failed to display {what}: {e}");
    }
    text
}
