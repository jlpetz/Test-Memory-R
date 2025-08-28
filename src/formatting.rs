use std::time::Duration;
use crate::constants::{BYTES_PER_GIB_F64, BYTES_PER_MIB_F64, BYTES_PER_KIB_F64};

/// Format bytes as GiB with 2 decimal places
pub fn format_bytes_gib(bytes: usize) -> String {
    format!("{:.2} GiB", bytes as f64 / BYTES_PER_GIB_F64)
}

/// Format bytes as MiB with 2 decimal places
pub fn format_bytes_mib(bytes: usize) -> String {
    format!("{:.2} MiB", bytes as f64 / BYTES_PER_MIB_F64)
}

/// Format bytes as KiB with 2 decimal places
pub fn format_bytes_kib(bytes: usize) -> String {
    format!("{:.2} KiB", bytes as f64 / 1024.0)
}

/// Format bytes intelligently (auto-select unit)
pub fn format_bytes_auto(bytes: usize) -> String {
    let bytes_f = bytes as f64;
    if bytes_f >= BYTES_PER_GIB_F64 {
        format!("{:.2} GiB", bytes_f / BYTES_PER_GIB_F64)
    } else if bytes_f >= BYTES_PER_MIB_F64 {
        format!("{:.2} MiB", bytes_f / BYTES_PER_MIB_F64)
    } else if bytes_f >= BYTES_PER_KIB_F64 {
        format!("{:.2} KiB", bytes_f / BYTES_PER_KIB_F64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format duration as HH:MM:SS
pub fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}

/// Format duration in milliseconds with appropriate unit
pub fn format_duration_ms(ms: u128) -> String {
    if ms >= 60000 {
        let minutes = ms / 60000;
        let seconds = (ms % 60000) / 1000;
        format!("{}m {:02}s", minutes, seconds)
    } else if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}ms", ms)
    }
}

/// Format duration in microseconds with appropriate unit
pub fn format_duration_us(us: u128) -> String {
    if us >= 1_000_000 {
        format!("{:.3}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.3}ms", us as f64 / 1_000.0)
    } else {
        format!("{}μs", us)
    }
}

/// Format percentage with specified decimal places
pub fn format_percentage(value: f64, decimals: usize) -> String {
    format!("{:.prec$}%", value * 100.0, prec = decimals)
}

/// Format frequency (Hz) with appropriate unit
pub fn format_frequency(hz: f64) -> String {
    if hz >= 1_000_000_000.0 {
        format!("{:.2} GHz", hz / 1_000_000_000.0)
    } else if hz >= 1_000_000.0 {
        format!("{:.2} MHz", hz / 1_000_000.0)
    } else if hz >= 1_000.0 {
        format!("{:.2} kHz", hz / 1_000.0)
    } else {
        format!("{:.2} Hz", hz)
    }
}

/// Format number with thousands separators
pub fn format_number_with_separators(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    let len = s.len();
    
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(c);
    }
    result
}

/// Format memory speed (MT/s)
pub fn format_memory_speed(speed_mts: u32) -> String {
    format!("{} MT/s", format_number_with_separators(speed_mts as u64))
}

/// Format CPU speed
pub fn format_cpu_speed_mhz(speed_mhz: u32) -> String {
    if speed_mhz >= 1000 {
        format!("{:.2} GHz", speed_mhz as f64 / 1000.0)
    } else {
        format!("{} MHz", speed_mhz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::MB;
    
    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes_gib(BYTES_PER_GIB_F64 as usize), "1.00 GiB");
        assert_eq!(format_bytes_mib(MB), "1.00 MiB");
        assert_eq!(format_bytes_kib(1024), "1.00 KiB");
    }
    
    #[test]
    fn test_format_duration() {
        let duration = Duration::from_secs(3661); // 1 hour, 1 minute, 1 second
        assert_eq!(format_duration(duration), "01:01:01");
    }
    
    #[test]
    fn test_format_number_with_separators() {
        assert_eq!(format_number_with_separators(1234567), "1,234,567");
        assert_eq!(format_number_with_separators(123), "123");
    }
}