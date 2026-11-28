//! The last note of a run, flushed to disk while the window is showing one (the OCCT idea, without a picture of a hard crash). A reset
//! keeps the last note. The next open of the window shows it. A run that ends marks the note closed, so it is not shown again.

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// What the window last knew, small enough to rewrite every couple of seconds
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub open: bool,
    pub written: String,
    pub position: String,
    pub errors: String,
    pub readings: String,
    pub results: Option<String>,
}

/// One write's token. A later note, or a close, takes a newer one, so a heartbeat still in flight cannot reopen a run that has ended.
static EPOCH: AtomicU64 = AtomicU64::new(0);
/// One writer at a time: the token check and the flush are the same step
static WRITE: Mutex<()> = Mutex::new(());

/// A token for a write about to be spawned. Taken on the window's thread, before the write, so a close in between makes it stale.
pub fn ticket() -> u64 {
    EPOCH.fetch_add(1, Ordering::Relaxed) + 1
}

fn current() -> u64 {
    EPOCH.load(Ordering::Relaxed)
}

/// `logs/live.json` in TMR's home
pub fn path() -> PathBuf {
    tmr::home::logs().join("live.json")
}

/// Only one window for a home writes or acknowledges its shared journal.
pub fn claim() -> Option<File> {
    let path = path();
    std::fs::create_dir_all(path.parent()?).ok()?;
    OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(path.with_extension("lock")).ok()
}

/// The note, if a run was still going when it was written
pub fn unfinished() -> Option<Note> {
    read(&path()).filter(|note| note.open)
}

/// Rewrites the note and flushes it, on this thread. A failure is ignored: the run does not stop for a note.
pub fn save(note: &Note) {
    let _ = write_if(ticket(), &path(), note);
}

/// A heartbeat's write, carrying the token taken when it was spawned. A newer token has already won.
pub fn save_with(ticket: u64, note: &Note) {
    let _ = write_if(ticket, &path(), note);
}

/// The run ended, or the user has seen the note. The file stays, closed, so the next open does not ask again. A write still in flight,
/// taken before this, does not put the note back.
pub fn close() {
    let _guard = WRITE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    EPOCH.fetch_add(1, Ordering::Relaxed);
    let path = path();
    if let Some(mut note) = read(&path) {
        note.open = false;
        let _ = write(&path, &note);
    }
}

fn write_if(ticket: u64, path: &Path, note: &Note) -> Result<(), String> {
    let _guard = WRITE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if current() != ticket {
        return Ok(());
    }
    write(path, note)
}

pub fn write(path: &Path, note: &Note) -> Result<(), String> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    }
    let mut text = serde_json::to_string_pretty(&note_value(note)).map_err(|e| e.to_string())?;
    text.push('\n');
    tmr::atomic_file::write(path, text.as_bytes()).map(|_| ())
}

fn note_value(note: &Note) -> serde_json::Value {
    let mut value = serde_json::json!({
        "open": note.open,
        "written": note.written,
        "position": note.position,
        "errors": note.errors,
        "readings": note.readings,
    });
    if let Some(results) = &note.results {
        value["results"] = serde_json::Value::String(results.clone());
    }
    value
}

fn read(path: &Path) -> Option<Note> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(Note {
        open: value.get("open")?.as_bool()?,
        written: value.get("written")?.as_str()?.to_string(),
        position: value.get("position")?.as_str()?.to_string(),
        errors: value.get("errors")?.as_str()?.to_string(),
        readings: value.get("readings")?.as_str()?.to_string(),
        results: value.get("results").and_then(|v| v.as_str()).map(str::to_string),
    })
}

/// A note taken now
pub fn now(position: String, errors: String, readings: String, results: Option<String>) -> Note {
    Note { open: true, written: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), position, errors, readings, results }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(position: &str) -> Note {
        Note {
            open: true,
            written: "2026-11-25 18:00:00".into(),
            position: position.into(),
            errors: "2 errors".into(),
            readings: "Timings 30-38-38-56".into(),
            results: Some("results\\run.json".into()),
        }
    }

    #[test]
    fn a_flushed_note_is_unfinished_until_it_is_closed() {
        let dir = tmr::home::dir().join(format!("tmr-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("live.json");
        let note = sample("cycle 1");
        write(&path, &note).unwrap();
        assert_eq!(read(&path), Some(note.clone()));
        let mut closed = note;
        closed.open = false;
        write(&path, &closed).unwrap();
        assert!(!read(&path).unwrap().open);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_ticket_does_not_reopen_a_closed_note() {
        let dir = tmr::home::dir().join(format!("tmr-journal-epoch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("live.json");
        let note = sample("cycle 1");
        let first = ticket();
        let second = ticket();
        write_if(first, &path, &note).unwrap();
        assert!(read(&path).is_none(), "an older token does not land after a newer one was taken");
        write_if(second, &path, &note).unwrap();
        assert!(read(&path).unwrap().open);
        {
            let _guard = WRITE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            EPOCH.fetch_add(1, Ordering::Relaxed);
            let mut closed = note.clone();
            closed.open = false;
            write(&path, &closed).unwrap();
        }
        write_if(second, &path, &note).unwrap();
        assert!(!read(&path).unwrap().open, "a heartbeat still in flight does not reopen a closed run");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
