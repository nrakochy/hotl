//! The plan as durable state (0056 T2): the live nodes plus the decisions
//! log, and the on-disk artifact they render to.
//!
//! A session's plan used to die with the session. It is filed per *project*
//! now, so the next session in the same repo finds what the last one was
//! doing — which is the difference between "resume this log" and "pick this
//! work back up".

use std::path::Path;

use hotl_platform::PrivateFs as _;
use hotl_types::{Decision, Todo, TodoStatus};
use serde::{Deserialize, Serialize};

/// The plan as it stands: what the actor owns and every reader sees.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanState {
    pub todos: Vec<Todo>,
    pub decisions: Vec<Decision>,
}

impl PlanState {
    /// Is there anything to render? An empty plan is not a plan.
    pub fn is_empty(&self) -> bool {
        self.todos.is_empty() && self.decisions.is_empty()
    }

    /// The human half: the same marks the model reads, plus the decisions.
    /// One renderer — the artifact's `current.md`, the repo mirror and the
    /// compaction digest's plan block are all this text.
    pub fn markdown(&self) -> String {
        let mut s = String::from("# Plan\n\n");
        if let Some(hotl_types::Item::User { text, .. }) =
            hotl_tools::todo::render_reminder(&self.todos)
        {
            s.push_str(&text);
            s.push('\n');
        } else {
            s.push_str("(no open steps)\n");
        }
        if !self.decisions.is_empty() {
            s.push_str("\n## Decisions\n\n");
            for d in &self.decisions {
                s.push_str(&format!("- {} — {}\n", d.what, d.why));
            }
        }
        s
    }
}

/// The on-disk shape. `version` is its own number, independent of
/// `FORMAT_VERSION`: this file is not a session log and is not replayed —
/// a reader that does not know a version simply ignores the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanArtifact {
    pub version: u32,
    /// The project id this plan is filed under (`hotl_store::project::id`).
    pub project: String,
    /// The session that last wrote it — the trail back to the transcript.
    pub session: String,
    pub updated_ms: u64,
    pub nodes: Vec<Todo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
}

/// The current artifact version.
pub const ARTIFACT_VERSION: u32 = 1;

impl PlanArtifact {
    pub fn new(project: String, session: String, updated_ms: u64, state: &PlanState) -> Self {
        Self {
            version: ARTIFACT_VERSION,
            project,
            session,
            updated_ms,
            nodes: state.todos.clone(),
            decisions: state.decisions.clone(),
        }
    }

    /// The human half, via the one renderer [`PlanState::markdown`].
    pub fn markdown(&self) -> String {
        PlanState {
            todos: self.nodes.clone(),
            decisions: self.decisions.clone(),
        }
        .markdown()
    }
}

/// `(done, total)` over the nodes — what the resume reminder counts.
pub fn progress(nodes: &[Todo]) -> (usize, usize) {
    (
        nodes
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count(),
        nodes.len(),
    )
}

/// Any node still worth picking up. A list that is entirely `completed` is
/// finished work, not an unfinished plan — and `Failed`/`NeedsMoreSteps`
/// most certainly are open.
pub fn has_open_nodes(nodes: &[Todo]) -> bool {
    nodes.iter().any(|t| t.status != TodoStatus::Completed)
}

/// Write `current.json` + `current.md` into `dir`, temp-then-rename so a
/// crash mid-write never leaves a half-parsed plan behind. Errors are
/// swallowed by the caller: a plan artifact is a convenience, and failing a
/// `todo_write` because a data directory is read-only would be worse than
/// having no file.
pub fn write_artifact(dir: &Path, artifact: &PlanArtifact) -> std::io::Result<()> {
    hotl_platform::PRIVATE_FS.create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(artifact)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    atomic_write(&dir.join("current.json"), json.as_bytes())?;
    atomic_write(&dir.join("current.md"), artifact.markdown().as_bytes())
}

/// The repo mirror (`[plan] repo_dir`): the markdown only. The JSON is
/// harness state and has no business in someone's commit.
pub fn mirror_markdown(path: &Path, artifact: &PlanArtifact) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Plain fs, not `PRIVATE_FS`: this one is meant to be readable — it is a
    // file in the user's repo, next to files they wrote themselves.
    std::fs::write(path, artifact.markdown())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut f = hotl_platform::PRIVATE_FS.create_file_truncate(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    std::fs::rename(&tmp, path)
}

/// Read a project's artifact back, if one parses.
pub fn read_artifact(dir: &Path) -> Option<PlanArtifact> {
    let text = std::fs::read_to_string(dir.join("current.json")).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(content: &str, status: TodoStatus) -> Todo {
        Todo {
            content: content.into(),
            status,
            ..Todo::default()
        }
    }

    #[test]
    fn the_artifact_round_trips_and_renders_both_halves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = PlanState {
            todos: vec![
                node("build", TodoStatus::Completed),
                node("test", TodoStatus::Pending),
            ],
            decisions: vec![Decision {
                when_ms: 7,
                what: "pinned serde".into(),
                why: "1.0.200 broke the derive".into(),
            }],
        };
        let a = PlanArtifact::new("proj".into(), "sess".into(), 42, &state);
        write_artifact(dir.path(), &a).expect("write");
        assert_eq!(read_artifact(dir.path()).as_ref(), Some(&a));
        let md = std::fs::read_to_string(dir.path().join("current.md")).expect("md");
        assert!(md.contains("[x] build"), "{md}");
        assert!(md.contains("## Decisions"), "{md}");
        assert!(
            md.contains("pinned serde — 1.0.200 broke the derive"),
            "{md}"
        );
        // No `.tmp` survives a completed write.
        assert!(!dir.path().join("current.tmp").exists());
    }

    #[test]
    fn open_nodes_is_anything_not_completed() {
        assert!(!has_open_nodes(&[node("a", TodoStatus::Completed)]));
        assert!(has_open_nodes(&[node("a", TodoStatus::Failed)]));
        assert!(has_open_nodes(&[node("a", TodoStatus::NeedsMoreSteps)]));
        assert_eq!(
            progress(&[
                node("a", TodoStatus::Completed),
                node("b", TodoStatus::Pending)
            ]),
            (1, 2)
        );
    }
}
