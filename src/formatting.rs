use std::time::Duration;
use crate::constants::{BYTES_PER_GIB_F64, BYTES_PER_MIB_F64, BYTES_PER_KIB_F64};

// ============================================================================
// JSON presentation — how numbers are written into result/config files
// ============================================================================

/// A byte count that reads as a human unit in JSON and parses back **exactly**.
///
/// `"l3_cache": 503316480` makes a reader do long division to answer "is this the same CPU?".
/// Cache sizes, alignments and addresses have an unambiguous natural unit (nobody wants L3 in KiB),
/// so a single auto-scaled field beats both a raw byte count and a bytes+display pair — the pair
/// doubles the line count for no new information.
///
/// The safety property: it only uses a unit that divides the value **exactly**, so no rounding is
/// ever involved and `Deserialize` recovers the original integer. A value with no clean unit
/// (`49153`) is written as `"49153 B"` rather than quietly rounded to `"48 KiB"`. That is why this
/// is not `format_bytes_auto`, which is fixed at 2 decimals and therefore lossy above ~10 MiB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(pub u64);

impl ByteSize {
    /// Largest binary unit that divides the value exactly, or bytes if none does.
    fn split_unit(self) -> (u64, &'static str) {
        const UNITS: [(u64, &str); 4] = [
            (1 << 30, "GiB"),
            (1 << 20, "MiB"),
            (1 << 10, "KiB"),
            (1, "B"),
        ];
        for (scale, suffix) in UNITS {
            if self.0 >= scale && self.0.is_multiple_of(scale) {
                return (self.0 / scale, suffix);
            }
        }
        (self.0, "B")
    }
}

impl From<usize> for ByteSize {
    fn from(bytes: usize) -> Self {
        Self(bytes as u64)
    }
}

impl From<u64> for ByteSize {
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Display for ByteSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (value, suffix) = self.split_unit();
        write!(f, "{} {}", value, suffix)
    }
}

impl serde::Serialize for ByteSize {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for ByteSize {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let text = String::deserialize(d)?;
        let (number, suffix) = text.trim().split_once(' ').unwrap_or((text.trim(), "B"));
        let scale: u64 = match suffix.trim() {
            "B" => 1,
            "KiB" => 1 << 10,
            "MiB" => 1 << 20,
            "GiB" => 1 << 30,
            other => return Err(D::Error::custom(format!("unknown byte unit {:?}", other))),
        };
        let value: u64 = number
            .trim()
            .parse()
            .map_err(|_| D::Error::custom(format!("not a whole byte count: {:?}", number)))?;
        Ok(Self(value * scale))
    }
}

/// Serde helpers that trim float noise out of JSON output.
///
/// Full `f64` precision on a derived quantity is not information — `30538.906752411574` MiB/s is
/// resolution far below the run-to-run variance of the thing being measured, and it makes result
/// files hard to read. These round at *serialisation* time only, so in-memory arithmetic keeps full
/// precision.
///
/// Scales are chosen so the last digit still means something: whole MiB/s (sub-1 MiB/s on a
/// ~30,000 MiB/s figure is noise), 2 dp for GiB and GiB/s (~10 MiB), and 2 dp for nanoseconds
/// (~1% at L1 latency, where it is still a real difference).
pub fn serialize_round_int<S: serde::Serializer>(value: &f64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_i64(value.round() as i64)
}

pub fn serialize_round_2dp<S: serde::Serializer>(value: &f64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((value * 100.0).round() / 100.0)
}

pub fn serialize_round_3dp<S: serde::Serializer>(value: &f64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((value * 1000.0).round() / 1000.0)
}

pub fn serialize_round_opt_2dp<S: serde::Serializer>(
    value: &Option<f64>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => s.serialize_some(&((v * 100.0).round() / 100.0)),
        None => s.serialize_none(),
    }
}

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
    
    /// The whole point of `ByteSize` is that making the JSON readable does not make it lossy, so the
    /// test that matters is the round trip — including the awkward value (`49153`) that has no clean
    /// unit and must stay in bytes rather than being rounded to `48 KiB`.
    #[test]
    fn byte_size_round_trips_through_json() {
        for (bytes, text) in [
            (49152u64, "48 KiB"),      // L1d per core
            (2 * MB as u64, "2 MiB"),  // L2 per core
            (503_316_480, "480 MiB"),  // L3 — not a whole number of GiB
            (8_589_934_592, "8 GiB"),  // min_start_address
            (49153, "49153 B"),        // no exact unit: stays in bytes
            (0, "0 B"),
        ] {
            let json = serde_json::to_string(&ByteSize(bytes)).unwrap();
            assert_eq!(json, format!("\"{}\"", text), "serializing {}", bytes);
            let parsed: ByteSize = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.0, bytes, "round-tripping {}", bytes);
        }
    }

    #[test]
    fn test_format_number_with_separators() {
        assert_eq!(format_number_with_separators(1234567), "1,234,567");
        assert_eq!(format_number_with_separators(123), "123");
    }
}