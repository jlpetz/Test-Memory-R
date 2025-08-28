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
    fn render_info(&mut self, text: &str) -> Result<()>;
    
    /// Render a warning message
    fn render_warning(&mut self, text: &str) -> Result<()>;
    
    /// Render an error message
    fn render_error(&mut self, text: &str) -> Result<()>;
    
    /// Render a success message
    fn render_success(&mut self, text: &str) -> Result<()>;
    
    /// Render a progress update (may overwrite previous line)
    fn render_progress(&mut self, text: &str) -> Result<()>;
    
    /// Render a separator line
    fn render_separator(&mut self) -> Result<()>;
    
    /// Clear the current line (for progress updates)
    fn clear_line(&mut self) -> Result<()>;
    
    /// Flush any buffered output
    fn flush(&mut self) -> Result<()>;
}