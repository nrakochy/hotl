//! L5 — persistence, M0 slice: one append-only JSONL file per session.
//!
//! The log is permanent by design (log-first spine), which is exactly why
//! secrets are masked **at ingestion, before bytes land** (Sec #8, r2 R6):
//! a later cleanup pass can never reach what was already written. Durable-ack
//! commit semantics arrive with the M1 writer actor; M0 flushes per entry.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use hotl_types::{new_ulid, Entry, EntryPayload, SessionHeader, FORMAT_VERSION};

/// Ingestion-time sentinel masking: values of secret-named env vars are
/// replaced with `«masked:NAME»` in every serialized entry.
pub struct Masker {
    pairs: Vec<(String, String)>, // (secret value, replacement)
}

const SECRET_NAME_MARKERS: [&str; 7] =
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "AUTH"];
const MIN_SECRET_LEN: usize = 8;

impl Masker {
    pub fn from_env() -> Self {
        let mut pairs: Vec<(String, String)> = std::env::vars()
            .filter(|(name, value)| {
                value.len() >= MIN_SECRET_LEN
                    && SECRET_NAME_MARKERS.iter().any(|m| name.to_uppercase().contains(m))
            })
            .map(|(name, value)| (value, format!("«masked:{name}»")))
            .collect();
        // Longest first so a secret that contains another secret masks whole.
        pairs.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        Self { pairs }
    }

    pub fn empty() -> Self {
        Self { pairs: Vec::new() }
    }

    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (secret, replacement) in &self.pairs {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), replacement);
            }
        }
        out
    }

    pub fn contains_secret(&self, text: &str) -> bool {
        self.pairs.iter().any(|(secret, _)| text.contains(secret.as_str()))
    }
}

/// Secrets-at-rest audit (M2; Sec #8 second half): scan existing session
/// logs for *current* secret values — entries written before a value became
/// a secret (or before masking existed) can't be rewritten in an append-only
/// store, so the honest remedy is a loud warning and rotation.
pub fn audit_secrets(dir: &Path, masker: &Masker) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut hits = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if masker.contains_secret(&content) {
                hits.push(path);
            }
        }
    }
    hits.sort();
    hits
}

pub struct SessionLog {
    file: File,
    path: PathBuf,
    masker: Masker,
    last_id: Option<String>,
    pub session_id: String,
}

impl SessionLog {
    /// Create `<dir>/<ulid>.jsonl` and write the header entry.
    pub fn create(
        dir: &Path,
        model: &str,
        parent_session_id: Option<String>,
        masker: Masker,
        now_ms: u64,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let session_id = new_ulid();
        let path = dir.join(format!("{session_id}.jsonl"));
        let file = OpenOptions::new().create_new(true).append(true).open(&path)?;
        let mut log = Self { file, path, masker, last_id: None, session_id: session_id.clone() };
        log.append(
            EntryPayload::Header {
                header: SessionHeader {
                    format_version: FORMAT_VERSION,
                    session_id,
                    parent_session_id,
                    model: model.to_string(),
                    created_at_ms: now_ms,
                },
            },
            now_ms,
        )?;
        Ok(log)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry (chained via parent_id), masked, flushed.
    pub fn append(&mut self, payload: EntryPayload, now_ms: u64) -> std::io::Result<String> {
        let entry = Entry {
            id: new_ulid(),
            parent_id: self.last_id.clone(),
            ts_ms: now_ms,
            payload,
        };
        let line = serde_json::to_string(&entry)
            .map_err(|e| std::io::Error::other(format!("serialize entry: {e}")))?;
        let masked = self.masker.apply(&line);
        self.file.write_all(masked.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.last_id = Some(entry.id.clone());
        Ok(entry.id)
    }
}

/// Reconstruct the projection from a session log (M3b): items append,
/// compactions and branch moves re-point, supersede digests append. This is
/// the replay half of log-first — the projection is always derivable.
pub struct Replayed {
    pub header: hotl_types::SessionHeader,
    pub items: Vec<hotl_types::Item>,
}

pub fn replay(path: &Path) -> Result<Replayed, String> {
    let mut items = Vec::new();
    let header = apply_log(path, &mut items)?;
    Ok(Replayed { header, items })
}

/// Replay a session *and its ancestry*: a resumed session's log starts from
/// the parent's projection, so entry indices (compaction, branch moves) are
/// relative to inherited-plus-own items. Cycle/depth capped at 32.
pub fn replay_chain(dir: &Path, session_id: &str) -> Result<Replayed, String> {
    let mut lineage = Vec::new();
    let mut current = session_id.to_string();
    for _ in 0..32 {
        let path = dir.join(format!("{current}.jsonl"));
        let raw = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let first: Entry = raw
            .lines()
            .next()
            .ok_or_else(|| format!("{}: empty log", path.display()))
            .and_then(|l| serde_json::from_str(l).map_err(|e| format!("{}: {e}", path.display())))?;
        let EntryPayload::Header { header } = first.payload else {
            return Err(format!("{}: first entry is not a header", path.display()));
        };
        let parent = header.parent_session_id.clone();
        lineage.push((path, header));
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }
    let (_, newest_header) = lineage.first().cloned().ok_or("empty lineage")?;
    let mut items = Vec::new();
    for (path, _) in lineage.iter().rev() {
        apply_log(path, &mut items)?;
    }
    Ok(Replayed { header: newest_header, items })
}

/// Apply one log's entries onto an existing projection; returns its header.
fn apply_log(path: &Path, items: &mut Vec<hotl_types::Item>) -> Result<hotl_types::SessionHeader, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut header = None;
    for (n, line) in raw.lines().enumerate() {
        let entry: Entry = serde_json::from_str(line)
            .map_err(|e| format!("{}:{} unparseable entry: {e}", path.display(), n + 1))?;
        match entry.payload {
            EntryPayload::Header { header: h } => header = Some(h),
            EntryPayload::Item { item } => items.push(item),
            EntryPayload::Compaction { digest, prefix_end, kept_from, .. } => {
                let prefix_end = prefix_end.min(items.len());
                let kept_from = kept_from.clamp(prefix_end, items.len());
                let tail = items.split_off(kept_from);
                items.truncate(prefix_end);
                items.extend(digest);
                items.extend(tail);
            }
            EntryPayload::BranchMove { keep_items } => items.truncate(keep_items),
            EntryPayload::Supersede { digest } => items.extend(digest),
            EntryPayload::Usage { .. } | EntryPayload::Cancelled { .. } | EntryPayload::Unknown => {}
        }
    }
    header.ok_or_else(|| format!("{}: no header entry", path.display()))
}

/// Session files in `dir`, newest first: (session id, path, modified).
pub fn list_sessions(dir: &Path) -> Vec<(String, PathBuf, std::time::SystemTime)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "jsonl" {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((id, path, modified))
        })
        .collect();
    out.sort_by_key(|(_, _, modified)| std::cmp::Reverse(*modified));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::Item;

    #[test]
    fn log_appends_chain_and_masks_secrets() {
        // A "secret" that will appear in a tool result.
        std::env::set_var("HOTL_TEST_API_KEY", "sk-super-secret-12345");
        let masker = Masker::from_env();
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create(dir.path(), "test-model", None, masker, 1000).unwrap();

        log.append(
            EntryPayload::Item {
                item: Item::User {
                    text: "here is the key: sk-super-secret-12345".into(),
                    synthetic: None,
                },
            },
            1001,
        )
        .unwrap();

        let content = std::fs::read_to_string(log.path()).unwrap();
        assert!(!content.contains("sk-super-secret-12345"), "secret leaked into the log");
        assert!(content.contains("«masked:HOTL_TEST_API_KEY»"));

        // Entries chain: line 2's parent is line 1's id.
        let lines: Vec<Entry> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(matches!(lines[0].payload, EntryPayload::Header { .. }));
        assert_eq!(lines[1].parent_id.as_ref(), Some(&lines[0].id));
        std::env::remove_var("HOTL_TEST_API_KEY");
    }

    #[test]
    fn replay_applies_items_compaction_and_branch_moves() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 1).unwrap();
        let user = |t: &str| Item::User { text: t.into(), synthetic: None };
        for text in ["one", "two", "three", "four"] {
            log.append(EntryPayload::Item { item: user(text) }, 2).unwrap();
        }
        // Compaction: fold [0..2) into a digest, keep the tail.
        log.append(
            EntryPayload::Compaction {
                digest: vec![user("digest-of-one-two")],
                prefix_end: 0,
                kept_from: 2,
                degraded: false,
            },
            3,
        )
        .unwrap();
        // Projection now: [digest, three, four]. Roll back to 2 items, record why.
        log.append(EntryPayload::BranchMove { keep_items: 2 }, 4).unwrap();
        log.append(
            EntryPayload::Supersede { digest: vec![user("abandoned: four")] },
            5,
        )
        .unwrap();

        let replayed = replay(log.path()).expect("replay");
        assert_eq!(replayed.header.model, "m");
        let texts: Vec<_> = replayed
            .items
            .iter()
            .map(|i| match i {
                Item::User { text, .. } => text.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(texts, ["digest-of-one-two", "three", "abandoned: four"]);

        let sessions = list_sessions(dir.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, replayed.header.session_id);
    }

    #[test]
    fn audit_finds_pre_masking_leaks() {
        let dir = tempfile::tempdir().unwrap();
        // A log written before `leaked-value-9` became a secret.
        std::fs::write(dir.path().join("old.jsonl"), r#"{"text":"key is leaked-value-9"}"#).unwrap();
        std::fs::write(dir.path().join("clean.jsonl"), r#"{"text":"nothing here"}"#).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "leaked-value-9").unwrap();

        let masker = Masker { pairs: vec![("leaked-value-9".into(), "«masked:X»".into())] };
        let hits = audit_secrets(dir.path(), &masker);
        assert_eq!(hits.len(), 1, "only the jsonl with the live secret");
        assert!(hits[0].ends_with("old.jsonl"));
        assert!(audit_secrets(dir.path(), &Masker::empty()).is_empty());
    }
}
