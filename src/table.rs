pub struct TableBuilder {
    headers: Vec<(String, Alignment)>,
    rows: Vec<Vec<String>>,
    separator_char: char,
    min_column_width: usize,
}

#[derive(Clone, Copy)]
pub enum Alignment {
    Left,
    Right,
    Center,
}

impl Default for TableBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TableBuilder {
    pub fn new() -> Self {
        Self {
            headers: Vec::new(),
            rows: Vec::new(),
            separator_char: '-',
            min_column_width: 10,
        }
    }
    
    pub fn add_header(mut self, header: &str, alignment: Alignment) -> Self {
        self.headers.push((header.to_string(), alignment));
        self
    }
    
    pub fn add_row(mut self, row: Vec<String>) -> Self {
        self.rows.push(row);
        self
    }
    
    pub fn print(&self) {
        if self.headers.is_empty() {
            return;
        }
        
        // Calculate column widths
        let mut widths: Vec<usize> = self.headers
            .iter()
            .map(|(h, _)| h.len().max(self.min_column_width))
            .collect();
        
        // Adjust widths based on row content
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(cell.len());
                }
            }
        }
        
        // Store original widths for content formatting
        let content_widths = widths.clone();
        
        // Add padding for separator calculation
        for width in &mut widths {
            *width += 2; // Add padding for separator width calculation
        }
        
        // Print separator
        let total_width: usize = widths.iter().sum();
        println!("{}", self.separator_char.to_string().repeat(total_width));
        
        // Print headers with proper spacing
        for (i, (header, alignment)) in self.headers.iter().enumerate() {
            let content_width = content_widths[i];
            let formatted = match alignment {
                Alignment::Left => format!("{:<width$}", header, width = content_width),
                Alignment::Right => format!("{:>width$}", header, width = content_width),
                Alignment::Center => format!("{:^width$}", header, width = content_width),
            };
            print!("{}  ", formatted); // Add 2 spaces between columns
        }
        println!();
        
        // Print separator
        println!("{}", self.separator_char.to_string().repeat(total_width));
        
        // Print rows with proper spacing
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < self.headers.len() {
                    let (_, alignment) = &self.headers[i];
                    let content_width = content_widths[i];
                    let formatted = match alignment {
                        Alignment::Left => format!("{:<width$}", cell, width = content_width),
                        Alignment::Right => format!("{:>width$}", cell, width = content_width),
                        Alignment::Center => format!("{:^width$}", cell, width = content_width),
                    };
                    print!("{}  ", formatted); // Add 2 spaces between columns
                }
            }
            println!();
        }
        
        // Print final separator
        println!("{}", self.separator_char.to_string().repeat(total_width));
    }
    
    // Alternative: return as string for logging - use format!("{}", table) via Display trait
}

impl std::fmt::Display for TableBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.headers.is_empty() {
            return Ok(());
        }
        
        // Calculate column widths
        let mut widths: Vec<usize> = self.headers
            .iter()
            .map(|(h, _)| h.len().max(self.min_column_width))
            .collect();
        
        // Adjust widths based on row content
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if let Some(width) = widths.get_mut(i) {
                    *width = (*width).max(cell.len());
                }
            }
        }
        
        let total_width = widths.iter().sum::<usize>() + widths.len() * 3 + 1;
        
        // Top border
        writeln!(f, "{}", self.separator_char.to_string().repeat(total_width))?;
        
        // Headers
        write!(f, "|")?;
        for ((header, align), &width) in self.headers.iter().zip(&widths) {
            write!(f, " ")?;
            match align {
                Alignment::Left => write!(f, "{:<width$}", header, width = width)?,
                Alignment::Right => write!(f, "{:>width$}", header, width = width)?,
                Alignment::Center => {
                    let padding = width.saturating_sub(header.len());
                    let left_pad = padding / 2;
                    let right_pad = padding - left_pad;
                    write!(f, "{}{}{}", " ".repeat(left_pad), header, " ".repeat(right_pad))?;
                }
            }
            write!(f, " |")?;
        }
        writeln!(f)?;
        
        // Header separator
        writeln!(f, "{}", self.separator_char.to_string().repeat(total_width))?;
        
        // Rows
        for row in &self.rows {
            write!(f, "|")?;
            for (i, (cell, &width)) in row.iter().zip(&widths).enumerate() {
                write!(f, " ")?;
                if let Some((_, align)) = self.headers.get(i) {
                    match align {
                        Alignment::Left => write!(f, "{:<width$}", cell, width = width)?,
                        Alignment::Right => write!(f, "{:>width$}", cell, width = width)?,
                        Alignment::Center => {
                            let padding = width.saturating_sub(cell.len());
                            let left_pad = padding / 2;
                            let right_pad = padding - left_pad;
                            write!(f, "{}{}{}", " ".repeat(left_pad), cell, " ".repeat(right_pad))?;
                        }
                    }
                } else {
                    write!(f, "{:<width$}", cell, width = width)?;
                }
                write!(f, " |")?;
            }
            writeln!(f)?;
        }
        
        // Final separator
        writeln!(f, "{}", self.separator_char.to_string().repeat(total_width))?;
        
        Ok(())
    }
}