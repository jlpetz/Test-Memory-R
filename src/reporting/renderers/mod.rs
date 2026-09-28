/// Renderer trait and implementations for different output targets
pub mod console;

pub use console::ConsoleRenderer;

use crate::Result;
use super::models::TableData;

/// Trait for rendering report data to various outputs
pub trait Renderer: Send {
    /// Render a table
    fn render_table(&mut self, table: &TableData) -> Result<()>;
    
    /// Render a heading
    fn render_heading(&mut self, text: &str) -> Result<()>;
    
    /// Render an info message
    #[expect(dead_code, reason = "TODO #29: kept as severity levels for the event-stream renderers")]
    fn render_info(&mut self, text: &str) -> Result<()>;

    /// Render a warning message
    fn render_warning(&mut self, text: &str) -> Result<()>;

    /// Render an error message
    #[expect(dead_code, reason = "TODO #29: kept as severity levels for the event-stream renderers")]
    fn render_error(&mut self, text: &str) -> Result<()>;

    /// Render a success message
    #[expect(dead_code, reason = "TODO #29: kept as severity levels for the event-stream renderers")]
    fn render_success(&mut self, text: &str) -> Result<()>;

    /// Render a separator line
    fn render_separator(&mut self) -> Result<()>;
}