//! On-disk prompt history for the interactive console — the I/O half of the
//! recall feature (the pure navigation/search core lives in `hotl-tui`).
//!
//! Format is JSONL: one JSON-encoded string per line, so embedded newlines in
//! multi-line prompts round-trip safely (a bash-style plain-newline file would
//! split them). The file is self-bounding: at startup it is trimmed to satisfy
//! *both* caps (entry count and byte size, smaller wins) and rewritten
//! atomically only when something was actually dropped. Steady state is an
//! O(1) append per submitted prompt, with consecutive-duplicate suppression.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::HistoryCfg;

/// A handle to the history file plus the caps it enforces. `path == None`
/// means history is disabled: load yields nothing and append is a no-op, but
/// in-session recall still works off the editor's in-memory ring.
pub struct History {
    path: Option<PathBuf>,
    /// The last entry written, for consecutive-dedup without re-reading disk.
    last: Option<String>,
}

impl History {
    /// Load the (bounded) tail of the history file and return a handle for
    /// subsequent appends. When disabled, returns an empty ring and a handle
    /// that never touches disk. Compaction happens here: if the caps drop any
    /// entries (or malformed lines are found), the file is rewritten in place.
    pub fn load(cfg: &HistoryCfg, data_dir: &Path) -> (Self, Vec<String>) {
        if !cfg.is_enabled() {
            return (
                History {
                    path: None,
                    last: None,
                },
                Vec::new(),
            );
        }
        let path = cfg.resolved_path(data_dir);
        let entries = read_and_bound(&path, cfg.max_entries(), cfg.max_bytes());
        let last = entries.last().cloned();
        (
            History {
                path: Some(path),
                last,
            },
            entries,
        )
    }

    /// Append one submitted prompt. No-op when disabled, when the text is
    /// blank, or when it equals the immediately previous entry (shell-style
    /// consecutive dedup). Best-effort: an I/O error is swallowed — losing a
    /// history line must never interrupt the session.
    pub fn append(&mut self, text: &str) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if text.trim().is_empty() || self.last.as_deref() == Some(text) {
            return;
        }
        let Ok(line) = serde_json::to_string(text) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let ok = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{line}"))
            .is_ok();
        if ok {
            // A very long-lived session could still overrun the caps; startup
            // compaction reclaims it on the next launch (mid-session compaction
            // is a deliberate non-goal).
            self.last = Some(text.to_string());
        }
    }
}

/// Read the file, parse JSONL string entries, and trim to both caps. Rewrites
/// the file atomically iff entries were dropped or malformed lines skipped.
fn read_and_bound(path: &Path, max_entries: usize, max_bytes: u64) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let raw_lines = text.lines().filter(|l| !l.trim().is_empty()).count();
    let parsed: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<String>(l).ok())
        .collect();
    let bounded = bound(&parsed, max_entries, max_bytes);
    // Rewrite when the caps dropped entries, or a malformed line was skipped —
    // both make the on-disk file diverge from `bounded`.
    if bounded.len() != raw_lines {
        let _ = atomic_rewrite(path, &bounded);
    }
    bounded
}

/// Keep the newest entries that fit under *both* caps. Walks newest→oldest,
/// accumulating serialized byte cost (line + newline); the most recent entry
/// is always kept even if it alone exceeds the byte cap.
fn bound(entries: &[String], max_entries: usize, max_bytes: u64) -> Vec<String> {
    let mut kept: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut bytes: u64 = 0;
    for e in entries.iter().rev() {
        if kept.len() >= max_entries {
            break;
        }
        let cost = serde_json::to_string(e)
            .map(|s| s.len() as u64 + 1)
            .unwrap_or(0);
        if !kept.is_empty() && bytes + cost > max_bytes {
            break;
        }
        bytes += cost;
        kept.push_front(e.clone());
    }
    kept.into()
}

/// Write to a temp file in the same directory, then rename over the original —
/// a reader never sees a half-written file.
fn atomic_rewrite(path: &Path, entries: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    for e in entries {
        body.push_str(&serde_json::to_string(e).unwrap_or_else(|_| "\"\"".into()));
        body.push('\n');
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("history"),
        std::process::id()
    ));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &Path, max_entries: usize, max_bytes: u64) -> HistoryCfg {
        HistoryCfg {
            enabled: Some(true),
            path: Some(dir.join("history.jsonl").to_string_lossy().into_owned()),
            max_entries: Some(max_entries),
            max_bytes: Some(max_bytes),
        }
    }

    /// Write a canonical JSONL file directly (bypassing append) for load tests.
    fn seed(path: &Path, entries: &[&str]) {
        let mut body = String::new();
        for e in entries {
            body.push_str(&serde_json::to_string(e).unwrap());
            body.push('\n');
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn jsonl_round_trips_including_multiline_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let (mut h, loaded) = History::load(&cfg(data, 100, 1 << 20), data);
        assert!(loaded.is_empty());
        h.append("write a test for foo");
        h.append("line one\nline two");
        // Reload from disk: order preserved, embedded newline intact.
        let (_h, reloaded) = History::load(&cfg(data, 100, 1 << 20), data);
        assert_eq!(reloaded, vec!["write a test for foo", "line one\nline two"]);
    }

    #[test]
    fn disabled_reads_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        seed(&data.join("history.jsonl"), &["old one", "old two"]);
        let disabled = HistoryCfg {
            enabled: Some(false),
            path: Some(data.join("history.jsonl").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let (mut h, loaded) = History::load(&disabled, data);
        assert!(loaded.is_empty(), "disabled load reads nothing");
        h.append("should not be written");
        // The pre-existing file is untouched (no read, no rewrite, no append).
        let raw = std::fs::read_to_string(data.join("history.jsonl")).unwrap();
        assert!(!raw.contains("should not be written"));
        assert!(raw.contains("old two"));
    }

    #[test]
    fn startup_trims_to_max_entries_and_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let path = data.join("history.jsonl");
        seed(&path, &["a", "b", "c", "d", "e"]);
        let (_h, loaded) = History::load(&cfg(data, 3, 1 << 20), data);
        assert_eq!(loaded, vec!["c", "d", "e"], "newest 3 kept");
        // The on-disk file is rewritten to match (self-bounding).
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 3);
        assert!(!raw.contains("\"a\"") && raw.contains("\"e\""));
    }

    #[test]
    fn startup_trims_to_max_bytes_independently() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let path = data.join("history.jsonl");
        // Each line serializes to `"xxxx"\n` = 7 bytes. A 16-byte cap fits two.
        seed(&path, &["xxxx", "yyyy", "zzzz"]);
        let (_h, loaded) = History::load(&cfg(data, 1000, 16), data);
        assert_eq!(loaded, vec!["yyyy", "zzzz"], "entry count high, bytes bind");
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    #[test]
    fn newest_entry_survives_even_if_it_alone_exceeds_the_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let path = data.join("history.jsonl");
        seed(
            &path,
            &["small", "a very long final entry that exceeds the cap"],
        );
        let (_h, loaded) = History::load(&cfg(data, 1000, 4), data);
        assert_eq!(loaded, vec!["a very long final entry that exceeds the cap"]);
    }

    #[test]
    fn append_suppresses_consecutive_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let (mut h, _) = History::load(&cfg(data, 100, 1 << 20), data);
        h.append("same");
        h.append("same");
        h.append("other");
        h.append("same");
        let (_h, reloaded) = History::load(&cfg(data, 100, 1 << 20), data);
        assert_eq!(reloaded, vec!["same", "other", "same"]);
    }

    #[test]
    fn no_rewrite_when_nothing_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let path = data.join("history.jsonl");
        seed(&path, &["a", "b"]);
        let before = std::fs::read_to_string(&path).unwrap();
        let (_h, loaded) = History::load(&cfg(data, 100, 1 << 20), data);
        assert_eq!(loaded, vec!["a", "b"]);
        // Within caps → file left byte-for-byte alone.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn malformed_lines_are_skipped_and_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let path = data.join("history.jsonl");
        std::fs::write(&path, "\"good\"\nnot json\n\"also good\"\n").unwrap();
        let (_h, loaded) = History::load(&cfg(data, 100, 1 << 20), data);
        assert_eq!(loaded, vec!["good", "also good"]);
        // The junk line is dropped from disk on the compacting rewrite.
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }
}
