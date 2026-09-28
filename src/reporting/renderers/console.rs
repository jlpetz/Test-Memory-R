/// Console renderer implementation using existing table.rs and terminal output
use super::Renderer;
use crate::Result;
use crate::table::{TableBuilder, Alignment};
use crate::reporting::models::{TableData, ColumnAlignment};

/// Console renderer for terminal output
pub struct ConsoleRenderer {
    use_colors: bool,
}

impl Default for ConsoleRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleRenderer {
    pub fn new() -> Self {
        Self {
            use_colors: true,
        }
    }

    /// Convert our alignment to table.rs alignment
    fn convert_alignment(align: ColumnAlignment) -> Alignment {
        match align {
            ColumnAlignment::Left => Alignment::Left,
            ColumnAlignment::Center => Alignment::Center,
            ColumnAlignment::Right => Alignment::Right,
        }
    }

    /// Get ANSI color code for different message types
    fn get_color_code(&self, msg_type: MessageType) -> &'static str {
        if !self.use_colors {
            return "";
        }

        match msg_type {
            MessageType::Info => "\x1b[36m",     // Cyan
            MessageType::Success => "\x1b[32m",   // Green
            MessageType::Warning => "\x1b[33m",   // Yellow
            MessageType::Error => "\x1b[31m",     // Red
            MessageType::Heading => "\x1b[1m",    // Bold
        }
    }

    fn reset_color(&self) -> &'static str {
        if self.use_colors { "\x1b[0m" } else { "" }
    }
}

enum MessageType {
    Info,
    Success,
    Warning,
    Error,
    Heading,
}

impl Renderer for ConsoleRenderer {
    fn render_table(&mut self, table: &TableData) -> Result<()> {
        // Print title if present (with newline separation)
        if let Some(title) = &table.title {
            println!("{}📊 {}{}", self.get_color_code(MessageType::Heading), title, self.reset_color());
        }

        // Build table using existing TableBuilder
        let mut builder = TableBuilder::new();

        // Add headers
        for header in &table.headers {
            builder = builder.add_header(&header.text, Self::convert_alignment(header.alignment));
        }

        // Add rows
        for row in &table.rows {
            builder = builder.add_row(row.clone());
        }

        // Print the table
        builder.print();

        // Print footer if present (directly connected to table)
        if let Some(footer) = &table.footer {
            println!("{}", footer);
        }
        // No automatic blank line - caller controls spacing

        Ok(())
    }

    fn render_heading(&mut self, text: &str) -> Result<()> {
        println!("\n{}🔍 {}{}", self.get_color_code(MessageType::Heading), text, self.reset_color());
        println!("{}", "═".repeat(80));
        Ok(())
    }

    fn render_info(&mut self, text: &str) -> Result<()> {
        println!("{}ℹ️  {}{}", self.get_color_code(MessageType::Info), text, self.reset_color());
        Ok(())
    }

    fn render_warning(&mut self, text: &str) -> Result<()> {
        println!("{}⚠️  {}{}", self.get_color_code(MessageType::Warning), text, self.reset_color());
        Ok(())
    }

    fn render_error(&mut self, text: &str) -> Result<()> {
        println!("{}❌ {}{}", self.get_color_code(MessageType::Error), text, self.reset_color());
        Ok(())
    }

    fn render_success(&mut self, text: &str) -> Result<()> {
        println!("{}✅ {}{}", self.get_color_code(MessageType::Success), text, self.reset_color());
        Ok(())
    }

    fn render_separator(&mut self) -> Result<()> {
        println!("\n{}", "=".repeat(80));
        Ok(())
    }
}
