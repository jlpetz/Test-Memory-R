//! Renderer trait for different output targets. The console's is the CLI's (`ConsoleRenderer`, crates/tmr-cli, TODO 29).

use super::models::TableData;
use crate::Result;

/// Trait for rendering report data to various outputs
pub trait Renderer: Send {
    /// Render a table
    fn render_table(&mut self, table: &TableData) -> Result<()>;

    /// Render a heading
    fn render_heading(&mut self, text: &str) -> Result<()>;

    /// Render a line of text as it is: a report's notes, a label, an empty line between tables
    fn render_text(&mut self, text: &str) -> Result<()>;

    /// Render an info message. Nothing calls it, nor `render_error` and `render_success`: they are kept as severity levels for the
    /// event-stream renderers (TODO 29). The trait is public since the console's renderer moved to the CLI's crate, so they aren't checked
    /// as dead code, and carry no `expect`.
    fn render_info(&mut self, text: &str) -> Result<()>;

    /// Render a warning message
    fn render_warning(&mut self, text: &str) -> Result<()>;

    /// Render an error message
    fn render_error(&mut self, text: &str) -> Result<()>;

    /// Render a success message
    fn render_success(&mut self, text: &str) -> Result<()>;

    /// Render a separator line
    fn render_separator(&mut self) -> Result<()>;
}
