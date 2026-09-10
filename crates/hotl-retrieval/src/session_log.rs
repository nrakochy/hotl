//! `recall` over the session's own log (0057 T5).
//!
//! The projection is what fits in the window; the **log** is the complete
//! record, and it outlives every fold. This backend indexes the log — so a
//! result the digest flattened, or the ladder cleared to a stub, is still
//! findable by id or by text.
//!
//! INVARIANT: a hit is never recall's own output. Indexing enveloped recall
//! results would let one search's excerpt be found by the next, compounding
//! untrusted text into itself. Enforced by `hits_exclude_recall_results`.

use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use hotl_types::{assistant_text, Entry, EntryPayload, Item};
use tokio_util::sync::CancellationToken;

use crate::{Hit, Query, Retriever, SourceRef};

/// What every hit from this backend says about itself. The log records what
/// was true when it was written, which is not the same claim as what is true
/// now — the whole reason a stale hit is a prompt to go and look.
pub const HISTORICAL_CLAUSE: &str =
    "historical, untrusted: it was true when written — verify against the workspace \
     before acting on it";

/// Bytes of one log item carried into an excerpt.
const EXCERPT_CAP: usize = 4_000;

/// How deep the ancestry walk goes. Mirrors `hotl_store::LINEAGE_DEPTH_CAP`
/// — a resumed session is a fork, and its history is its ancestors'.
const DEPTH_CAP: usize = 32;

pub struct SessionLogRetriever {
    dir: PathBuf,
    session_id: String,
}

impl SessionLogRetriever {
    pub fn new(dir: impl Into<PathBuf>, session_id: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            session_id: session_id.into(),
        }
    }

    /// This session's log then its ancestors', newest first. A missing or
    /// unreadable ancestor ends the walk: losing older history is a smaller
    /// failure than refusing to search at all.
    fn lineage(&self) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let mut id = self.session_id.clone();
        for _ in 0..DEPTH_CAP {
            if seen.contains(&id) {
                break; // a cycle in the parent chain
            }
            let path = self.dir.join(format!("{id}.jsonl"));
            if !path.is_file() {
                break;
            }
            seen.push(id.clone());
            let parent = parent_of(&path);
            out.push((id, path));
            match parent {
                Some(p) => id = p,
                None => break,
            }
        }
        out
    }
}

/// The `parent_session_id` in a log's header line, without reading the rest.
fn parent_of(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let entry: Entry = serde_json::from_str(first.trim_end()).ok()?;
    match entry.payload {
        EntryPayload::Header { header } => header.parent_session_id,
        _ => None,
    }
}

/// One searchable string per log item, labelled by role. `None` for anything
/// with no text a search could match — and for recall's own results.
fn searchable(item: &Item) -> Option<String> {
    match item {
        Item::User { text, .. } => Some(format!("[user] {text}")),
        Item::Assistant { blocks } => {
            let text = assistant_text(blocks);
            (!text.is_empty()).then(|| format!("[assistant] {text}"))
        }
        Item::ToolResults { results } => {
            let body: Vec<String> = results
                .iter()
                .filter(|r| !is_recall_result(&r.content))
                .map(|r| format!("[tool result {}] {}", r.tool_use_id, r.content))
                .collect();
            (!body.is_empty()).then(|| body.join("\n"))
        }
        Item::System { .. } | Item::Unknown => None,
    }
}

/// The recursion guard: recall's own results carry the sanitizer's
/// `source="recall:<backend>"` provenance, which nothing else writes.
fn is_recall_result(content: &str) -> bool {
    content.contains("source=\"recall:")
}

fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl Retriever for SessionLogRetriever {
    fn name(&self) -> &str {
        "session-log"
    }

    fn description(&self) -> &str {
        "this session's own history, including everything compaction folded away \
         or the context ladder cleared — search it by text, or by a tool_use id \
         a <cleared/> stub names"
    }

    fn clause(&self) -> Option<&str> {
        Some(HISTORICAL_CLAUSE)
    }

    fn search<'a>(
        &'a self,
        query: &'a Query,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            let needle = query.text.to_lowercase();
            if needle.trim().is_empty() {
                return Err("`query` cannot be empty for the session log.".into());
            }
            let mut hits = Vec::new();
            'logs: for (session, path) in self.lineage() {
                let body = std::fs::read_to_string(&path)
                    .map_err(|e| format!("read {}: {e}", path.display()))?;
                // Newest first: the most recent statement about a thing is the
                // one most likely still true.
                for line in body.lines().rev() {
                    if cancel.is_cancelled() {
                        break 'logs;
                    }
                    let Ok(entry) = serde_json::from_str::<Entry>(line) else {
                        continue;
                    };
                    let EntryPayload::Item { item } = entry.payload else {
                        continue;
                    };
                    let Some(text) = searchable(&item) else {
                        continue;
                    };
                    if !text.to_lowercase().contains(&needle) {
                        continue;
                    }
                    hits.push(Hit {
                        source: SourceRef::Session {
                            session: session.clone(),
                            entry: entry.id,
                        },
                        excerpt: clip(&text, EXCERPT_CAP).to_string(),
                        score: None,
                        indexed_at_unix: Some(entry.ts_ms / 1000),
                    });
                    if hits.len() >= query.k {
                        break 'logs;
                    }
                }
            }
            Ok(hits)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_K;
    use hotl_types::{SessionHeader, ToolResultItem};

    struct Log {
        dir: tempfile::TempDir,
        id: String,
        prev: Option<String>,
        n: u64,
    }

    impl Log {
        fn new(id: &str, parent: Option<&str>) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut log = Self {
                dir,
                id: id.into(),
                prev: None,
                n: 0,
            };
            log.append(EntryPayload::Header {
                header: SessionHeader {
                    format_version: 1,
                    session_id: id.into(),
                    parent_session_id: parent.map(str::to_string),
                    parent_tip_entry_id: None,
                    model: "m".into(),
                    created_at_ms: 1,
                },
            });
            log
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join(format!("{}.jsonl", self.id))
        }

        fn append(&mut self, payload: EntryPayload) -> String {
            self.n += 1;
            let id = format!("{}-{:03}", self.id, self.n);
            let entry = Entry {
                id: id.clone(),
                parent_id: self.prev.clone(),
                ts_ms: 1_700_000_000_000 + self.n,
                payload,
            };
            let line = serde_json::to_string(&entry).expect("serialize");
            let mut body = std::fs::read_to_string(self.path()).unwrap_or_default();
            body.push_str(&line);
            body.push('\n');
            std::fs::write(self.path(), body).expect("write log");
            self.prev = Some(id.clone());
            id
        }

        fn user(&mut self, text: &str) -> String {
            self.append(EntryPayload::Item {
                item: Item::User {
                    text: text.into(),
                    synthetic: None,
                    images: Vec::new(),
                },
            })
        }

        fn result(&mut self, tool_use_id: &str, content: &str) -> String {
            self.append(EntryPayload::Item {
                item: Item::ToolResults {
                    results: vec![ToolResultItem {
                        tool_use_id: tool_use_id.into(),
                        content: content.into(),
                        is_error: false,
                    }],
                },
            })
        }

        fn retriever(&self) -> SessionLogRetriever {
            SessionLogRetriever::new(self.dir.path(), &self.id)
        }
    }

    fn query(text: &str) -> Query {
        Query {
            text: text.into(),
            purpose: None,
            k: DEFAULT_K,
        }
    }

    async fn search(r: &SessionLogRetriever, text: &str) -> Vec<Hit> {
        r.search(&query(text), CancellationToken::new())
            .await
            .expect("search")
    }

    #[tokio::test]
    async fn session_log_backend_finds_a_result_by_id_and_by_text() {
        let mut log = Log::new("S1", None);
        log.user("run the tests");
        let entry = log.result("t7", "error[E0061]: wrong arity in crates/foo/src/bar.rs");
        let r = log.retriever();

        let by_text = search(&r, "E0061").await;
        assert_eq!(by_text.len(), 1);
        assert_eq!(by_text[0].source.to_string(), format!("session:S1#{entry}"));
        assert!(by_text[0].excerpt.contains("wrong arity"));

        // And by the id a `<cleared/>` stub would have named.
        let by_id = search(&r, "t7").await;
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].source.to_string(), format!("session:S1#{entry}"));
    }

    #[tokio::test]
    async fn hits_exclude_recall_results() {
        let mut log = Log::new("S1", None);
        log.result("t1", "the needle is here");
        log.result(
            "t2",
            "<tool-result source=\"recall:session-log\">the needle is here</tool-result>",
        );
        let hits = search(&log.retriever(), "needle").await;
        assert_eq!(
            hits.len(),
            1,
            "recall's own output is not indexed: {hits:?}"
        );
        assert!(hits[0].excerpt.contains("[tool result t1]"));
    }

    #[tokio::test]
    async fn hits_carry_the_historical_clause() {
        let log = Log::new("S1", None);
        assert_eq!(log.retriever().clause(), Some(HISTORICAL_CLAUSE));
        assert!(HISTORICAL_CLAUSE.contains("verify against the workspace"));
    }

    /// The log, not the projection, is what is indexed — so an item a fold
    /// re-pointed away is still found.
    #[tokio::test]
    async fn hits_cross_a_compaction_fold() {
        let mut log = Log::new("S1", None);
        log.user("the original instruction: never touch legacy");
        log.result("t1", "a detail the digest would flatten");
        log.append(EntryPayload::Compaction {
            digest: vec![Item::User {
                text: "<compaction-summary>GOAL: x</compaction-summary>".into(),
                synthetic: Some(hotl_types::SyntheticReason::CompactionSummary),
                images: Vec::new(),
            }],
            prefix_end: 0,
            kept_from: 2,
            degraded: false,
            pinned: Vec::new(),
            source_range: None,
        });
        // The projection no longer holds either item; the log still does.
        let replayed = hotl_store::replay(&log.path()).expect("replay");
        assert!(!replayed
            .items
            .iter()
            .any(|i| matches!(i, Item::User { text, .. } if text.contains("never touch legacy"))));

        let hits = search(&log.retriever(), "never touch legacy").await;
        assert_eq!(hits.len(), 1, "{hits:?}");
        let hits = search(&log.retriever(), "would flatten").await;
        assert_eq!(hits.len(), 1, "{hits:?}");
    }

    #[tokio::test]
    async fn the_walk_reaches_an_ancestor_and_stops_at_k() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut parent = Log::new("P1", None);
        let mut child = Log::new("C1", Some("P1"));
        parent.user("said in the parent session");
        for n in 0..5 {
            child.user(&format!("said in the child session {n}"));
        }
        // Move both logs into one directory, which is what a real store is.
        for src in [parent.path(), child.path()] {
            let dst = dir.path().join(src.file_name().expect("name"));
            std::fs::copy(&src, &dst).expect("copy");
        }
        let r = SessionLogRetriever::new(dir.path(), "C1");
        assert_eq!(search(&r, "said in the parent session").await.len(), 1);
        let capped = r
            .search(
                &Query {
                    text: "said in".into(),
                    purpose: None,
                    k: 2,
                },
                CancellationToken::new(),
            )
            .await
            .expect("search");
        assert_eq!(capped.len(), 2, "k caps the hits");
        // Newest first: the child's newest line comes back before the rest.
        assert!(capped[0].excerpt.contains("child session 4"), "{capped:?}");
    }

    #[tokio::test]
    async fn an_unknown_session_searches_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = SessionLogRetriever::new(dir.path(), "nope");
        assert!(search(&r, "anything").await.is_empty());
        assert!(r
            .search(&query("  "), CancellationToken::new())
            .await
            .is_err());
    }
}
