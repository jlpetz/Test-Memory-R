//! Timings and temperatures from tools the user already runs (TODO 122). ZenTimings' live snapshot, and LibreHardwareMonitor's local
//! web server. Neither is required: a miss is nothing, and the run goes on. No driver, no BIOS write, no GPL code.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a run found beside itself, at its start. Absent from the results file when neither source had a fresh reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalReading {
    /// One line for the console and the window.
    pub line: String,
    /// Primaries, `CL-RCD-RP-RAS`, when ZenTimings had them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<String>,
    /// A DIMM temperature, °C, when a source had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimm_c: Option<f64>,
}

/// The reading now, or nothing when both sources are absent or stale. A closed port costs the connect timeout, then nothing.
pub fn sample() -> Option<ExternalReading> {
    let zen = zen_timings();
    let temps = libre_hardware();
    if zen.is_none() && temps.is_none() {
        return None;
    }
    let (timings, dimm_c) = zen.unwrap_or((None, None));
    let dimm_c = dimm_c.or(temps);
    let mut parts = Vec::new();
    if let Some(timings) = &timings {
        parts.push(format!("Timings {timings}"));
    }
    if let Some(temp) = dimm_c {
        parts.push(format!("DIMM {temp:.0}°C"));
    }
    Some(ExternalReading { line: parts.join(" · "), timings, dimm_c })
}

/// `sample`'s line, or empty. The window's header uses this.
pub fn line() -> String {
    sample().map(|reading| reading.line).unwrap_or_default()
}

fn zen_timings() -> Option<(Option<String>, Option<f64>)> {
    let file = std::fs::File::open(snapshot_path()?).ok()?;
    let mut text = String::new();
    file.take((MAX_RESPONSE + 1) as u64).read_to_string(&mut text).ok()?;
    if text.len() > MAX_RESPONSE {
        return None;
    }
    parse_zen(&text, Utc::now())
}

fn parse_zen(text: &str, now: DateTime<Utc>) -> Option<(Option<String>, Option<f64>)> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.pointer("/readings/stale")?.as_bool()? {
        return None;
    }
    let sampled = DateTime::parse_from_rfc3339(value.pointer("/readings/sampled_at")?.as_str()?).ok()?;
    let age = now - sampled.with_timezone(&Utc);
    if age < TimeDelta::zero() || age > TimeDelta::seconds(30) {
        return None;
    }
    let timings = value.pointer("/config/timings/per_dct/0/timings").and_then(|timings| {
        let cl = number(timings, "CL")?;
        let rcd = number(timings, "RCDRD").or_else(|| number(timings, "RCD"))?;
        let rp = number(timings, "RP")?;
        let ras = number(timings, "RAS")?;
        Some(format!("{cl}-{rcd}-{rp}-{ras}"))
    });
    let dimm_c = value
        .pointer("/readings/dimm")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|dimm| dimm.get("spd_hub_temp_c").and_then(Value::as_f64).or_else(|| dimm.get("dimm_temp_c").and_then(Value::as_f64)))
        .max_by(f64::total_cmp);
    (timings.is_some() || dimm_c.is_some()).then_some((timings, dimm_c))
}

fn number(timings: &Value, key: &str) -> Option<i64> {
    timings.get(key).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|n| n as i64)))
}

fn snapshot_path() -> Option<PathBuf> {
    if let Some(set) = std::env::var_os("TMR_TIMINGS") {
        let path = PathBuf::from(set);
        if path.is_file() {
            return Some(path);
        }
    }
    let name = "live_snapshot_device_config.json";
    let beside = crate::companions::zen_snapshot();
    if beside.is_file() {
        return Some(beside);
    }
    for var in ["LOCALAPPDATA", "ProgramData"] {
        let Some(root) = std::env::var_os(var) else { continue };
        let path = PathBuf::from(root).join("ZenTimings").join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn libre_hardware() -> Option<f64> {
    let body = http_get("127.0.0.1:8085", "/data.json")?;
    parse_libre(&body)
}

fn parse_libre(body: &str) -> Option<f64> {
    let value: Value = serde_json::from_str(body).ok()?;
    warmest_dimm(&value, false)
}

fn warmest_dimm(node: &Value, in_dimm: bool) -> Option<f64> {
    let name = node.get("Text").and_then(Value::as_str).unwrap_or("");
    let in_dimm = in_dimm || name.to_ascii_lowercase().contains("dimm");
    let reading = if in_dimm && node.get("Type").and_then(Value::as_str) == Some("Temperature") {
        node.get("RawValue").and_then(|raw| {
            raw.as_f64().or_else(|| raw.as_str()?.trim().replace(',', ".").parse::<f64>().ok()).filter(|value| value.is_finite())
        })
    } else {
        None
    };
    node.get("Children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|child| warmest_dimm(child, in_dimm))
        .chain(reading)
        .max_by(f64::total_cmp)
}

const MAX_RESPONSE: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(200);

fn http_get(host: &str, path: &str) -> Option<String> {
    let addr: SocketAddr = host.parse().ok()?;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut stream = TcpStream::connect_timeout(&addr, REQUEST_TIMEOUT).ok()?;
    stream.set_write_timeout(Some(remaining(deadline)?)).ok()?;
    let request = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        stream.set_read_timeout(Some(remaining(deadline)?)).ok()?;
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        if buf.len() + read > MAX_RESPONSE {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    let text = String::from_utf8(buf).ok()?;
    let (header, body) = text.split_once("\r\n\r\n")?;
    (header.split_whitespace().nth(1) == Some("200")).then(|| body.to_string())
}

fn remaining(deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    (!left.is_zero()).then_some(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zen_uses_fresh_primaries_and_the_warmest_dimm() {
        let now = "2026-11-25T18:00:25Z".parse().unwrap();
        let fresh = r#"{"readings":{"sampled_at":"2026-11-25T18:00:00Z","stale":false,"dimm":[{"spd_hub_temp_c":42.4},{"spd_hub_temp_c":45}]},"config":{"timings":{"per_dct":[{"timings":{"CL":30,"RCDRD":38,"RP":38,"RAS":56}}]}}}"#;
        assert_eq!(parse_zen(fresh, now), Some((Some("30-38-38-56".into()), Some(45.0))));
        assert_eq!(parse_zen(&fresh.replace("false", "true"), now), None);
        assert_eq!(parse_zen(fresh, now + TimeDelta::seconds(6)), None);
    }

    #[test]
    fn libre_uses_raw_celsius_from_temperature_sensors() {
        let body = r#"{"Text":"PC","Children":[{"Text":"CPU","Type":"Temperature","RawValue":"70"},{"Text":"DIMM #1","Children":[{"Text":"Voltage","Type":"Voltage","RawValue":"90"},{"Text":"Temperature","Type":"Temperature","Value":"115.7 °F","RawValue":"46,5"}]}]}"#;
        assert_eq!(parse_libre(body), Some(46.5));
    }
}
