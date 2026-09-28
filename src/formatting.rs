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
/// (`49153`) is written as `"49153 B"` rather than quietly rounded to `"48 KiB"`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::MB;
    
    /// The whole point of `ByteSize` is that making the JSON readable does not make it lossy, so the
    /// test that matters is the round trip — including the awkward value (`49153`) that has no clean
    /// unit and must stay in bytes rather than being rounded to `48 KiB`.
    #[test]
    fn byte_size_round_trips_through_json() {
        for (bytes, text) in [
            (49152u64, "48 KiB"),      // L1d per core
            (2 * MB as u64, "2 MiB"),  // L2 per core
            (503_316_480, "480 MiB"),  // L3 — not a whole number of GiB
            (8_589_934_592, "8 GiB"),  // a whole number of GiB
            (49153, "49153 B"),        // no exact unit: stays in bytes
            (0, "0 B"),
        ] {
            let json = serde_json::to_string(&ByteSize(bytes)).unwrap();
            assert_eq!(json, format!("\"{}\"", text), "serializing {}", bytes);
            let parsed: ByteSize = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.0, bytes, "round-tripping {}", bytes);
        }
    }
}