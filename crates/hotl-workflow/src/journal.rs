//! The resume journal (0058 T10): one line per settled agent, keyed by what
//! that agent was actually asked.
//!
//! A workflow that dies on its fourth phase should not re-run the first
//! three. Replaying by *position* would be wrong — an edited `args` changes
//! what phase two is asked, and replaying the old answer would be a lie — so
//! the key is the content: phase, label, rendered prompt, schema. An
//! unchanged call replays; a changed one re-runs, and everything downstream
//! of it re-runs too, because its own prompt is templated from that answer.
//!
//! The *recipe* is guarded separately by a digest of the plan itself: resume
//! is for finishing the same work, not for grafting an old run's answers onto
//! a plan someone has since rewritten.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::plan::Plan;

/// The journal's file name inside a run's directory.
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// A digest of the plan as written. Changing any part of it — a phase title,
/// an agent's schema, the shape — makes an old run's journal inapplicable.
pub fn recipe_sha256(plan: &Plan) -> String {
    let canonical = serde_json::to_vec(plan).unwrap_or_default();
    hex(&Sha256::digest(&canonical))
}

/// What one agent call *was*, as a key. Rendered prompt and label, not the
/// templates they came from: two runs with different `args` are two
/// different calls even though the recipe is identical.
pub fn content_key(phase: &str, label: &str, prompt: &str, schema: Option<&Value>) -> String {
    let mut h = Sha256::new();
    // Length-prefixed, so ("ab", "c") and ("a", "bc") are different keys.
    for part in [phase, label, prompt] {
        h.update(part.len().to_le_bytes());
        h.update(part.as_bytes());
    }
    let schema = schema.map(|s| s.to_string()).unwrap_or_default();
    h.update(schema.len().to_le_bytes());
    h.update(schema.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One settled agent, as the journal records it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    /// `AgentStatus::as_str` — only `done` entries are ever replayed.
    pub status: String,
    pub reply: Value,
}

/// A run's journal: what has already been answered, and where to append.
///
/// `cached` is loaded once; `record` appends. A journal that cannot be
/// written degrades to no caching rather than failing the run — it is an
/// optimization, and losing it costs time, not correctness.
pub struct Journal {
    path: PathBuf,
    cached: HashMap<String, Value>,
    writer: Mutex<()>,
}

impl Journal {
    /// Load `<dir>/journal.jsonl`, ignoring lines that do not parse — a run
    /// killed mid-write leaves at most one torn tail, and one lost cache hit
    /// is cheaper than refusing to resume at all.
    pub fn load(dir: &Path) -> Journal {
        let path = dir.join(JOURNAL_FILE);
        let cached = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
            .filter(|e| e.status == "done")
            .map(|e| (e.key, e.reply))
            .collect();
        Journal {
            path,
            cached,
            writer: Mutex::new(()),
        }
    }

    /// An empty journal that records nothing — a fresh run with no directory.
    pub fn disabled() -> Journal {
        Journal {
            path: PathBuf::new(),
            cached: HashMap::new(),
            writer: Mutex::new(()),
        }
    }

    pub fn cached(&self, key: &str) -> Option<&Value> {
        self.cached.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.cached.is_empty()
    }

    /// Append one settled agent. Best effort: a journal that cannot be
    /// written just stops caching.
    pub fn record(&self, key: &str, status: &str, reply: &Value) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let line = match serde_json::to_string(&Entry {
            key: key.to_string(),
            status: status.to_string(),
            reply: reply.clone(),
        }) {
            Ok(l) => l,
            Err(_) => return,
        };
        let _guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_content_key_covers_every_part_and_cannot_be_confused_by_concatenation() {
        let a = content_key("Fix", "one", "do it", None);
        assert_eq!(a, content_key("Fix", "one", "do it", None), "stable");
        assert_ne!(a, content_key("Ship", "one", "do it", None));
        assert_ne!(a, content_key("Fix", "two", "do it", None));
        assert_ne!(a, content_key("Fix", "one", "do it twice", None));
        assert_ne!(
            a,
            content_key("Fix", "one", "do it", Some(&json!({"a": 1})))
        );
        // Length-prefixed: a boundary shift is a different key.
        assert_ne!(
            content_key("ab", "c", "p", None),
            content_key("a", "bc", "p", None)
        );
    }

    #[test]
    fn a_recipe_digest_moves_when_the_plan_does() {
        let plan = |title: &str| {
            Plan::from_json(json!({
                "name": "p",
                "phases": [{"title": title, "agents": [{"label": "a", "prompt": "p"}]}]
            }))
            .unwrap()
        };
        assert_eq!(recipe_sha256(&plan("A")), recipe_sha256(&plan("A")));
        assert_ne!(recipe_sha256(&plan("A")), recipe_sha256(&plan("B")));
    }

    #[test]
    fn only_done_entries_replay_and_a_torn_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::load(dir.path());
        assert!(j.is_empty());
        j.record("k1", "done", &json!("answered"));
        j.record("k2", "failed", &json!(null));
        // A torn tail, exactly as a killed process leaves one.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.path().join(JOURNAL_FILE))
                .unwrap();
            write!(f, "{{\"key\":\"k3\",\"stat").unwrap();
        }
        let reloaded = Journal::load(dir.path());
        assert_eq!(reloaded.cached("k1"), Some(&json!("answered")));
        assert_eq!(reloaded.cached("k2"), None, "a failure is not an answer");
        assert_eq!(reloaded.cached("k3"), None, "a torn line is skipped");
    }

    #[test]
    fn a_disabled_journal_caches_and_records_nothing() {
        let j = Journal::disabled();
        j.record("k", "done", &json!(1));
        assert!(j.is_empty() && j.cached("k").is_none());
    }
}
