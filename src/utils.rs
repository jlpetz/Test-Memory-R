

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
    
    pub fn separator_char(mut self, c: char) -> Self {
        self.separator_char = c;
        self
    }
    
    pub fn min_column_width(mut self, width: usize) -> Self {
        self.min_column_width = width;
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
        
        // Add padding
        for width in &mut widths {
            *width += 2; // Add padding
        }
        
        // Print separator
        let total_width: usize = widths.iter().sum();
        println!("{}", self.separator_char.to_string().repeat(total_width));
        
        // Print headers
        for (i, (header, alignment)) in self.headers.iter().enumerate() {
            let width = widths[i];
            let formatted = match alignment {
                Alignment::Left => format!("{:<width$}", header, width = width),
                Alignment::Right => format!("{:>width$}", header, width = width),
                Alignment::Center => format!("{:^width$}", header, width = width),
            };
            print!("{}", formatted);
        }
        println!();
        
        // Print separator
        println!("{}", self.separator_char.to_string().repeat(total_width));
        
        // Print rows
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < self.headers.len() {
                    let (_, alignment) = &self.headers[i];
                    let width = widths[i];
                    let formatted = match alignment {
                        Alignment::Left => format!("{:<width$}", cell, width = width),
                        Alignment::Right => format!("{:>width$}", cell, width = width),
                        Alignment::Center => format!("{:^width$}", cell, width = width),
                    };
                    print!("{}", formatted);
                }
            }
            println!();
        }
        
        // Print final separator
        println!("{}", self.separator_char.to_string().repeat(total_width));
    }
    
    // Alternative: return as string for logging
    pub fn to_string(&self) -> String {
        // Similar implementation but building a String instead of printing
        let mut result = String::new();
        // ... (same logic as print but using result.push_str)
        result
    }
}
