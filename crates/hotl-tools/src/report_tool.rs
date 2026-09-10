//! `report_result` (0058 T1): a sub-agent's typed, bounded return.
//!
//! Registered only in a child registry — a parent has nobody to report to.
//! The child writes `RESPONSE.json` beside its `TASK.md`; the parent reads
//! the file rather than parsing the child's final prose, so "what came back"
//! is a shape, not a guess.

use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use hotl_platform::PrivateFs;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{Permission, Tool, ToolOutcome};

/// ≈1,500 tokens. A longer summary is refused, never truncated: the child
/// still holds the material and can say less; a truncation would silently
/// drop the half the parent needed.
pub const MAX_SUMMARY_CHARS: usize = 6000;

/// The four outcomes a child may report. `unverifiable` is what the parent
/// synthesizes for a child that never reported at all, so it is a value the
/// child can also choose honestly.
pub const OUTCOMES: [&str; 4] = ["completed", "blocked", "needs_input", "unverifiable"];

/// The file a child's typed result lands in, inside its own spawn scratch dir.
pub const RESPONSE_FILE: &str = "RESPONSE.json";

/// The brief a child is pointed at, in the same dir.
pub const TASK_FILE: &str = "TASK.md";

pub struct ReportResultTool {
    /// `<data_dir>/spawn/<child-ulid>/RESPONSE.json`.
    path: PathBuf,
}

impl ReportResultTool {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

/// Read back what a child reported, or `None` if it never did.
pub fn read_response(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The result the parent invents for a child that ended without reporting:
/// its final text, marked as the unverified thing it is.
pub fn unverifiable(summary: &str) -> Value {
    json!({"outcome": "unverifiable", "summary": summary})
}

/// What a parent tells a child that finished without reporting.
pub const REPROMPT: &str = "Call report_result now with what you have.";

fn string_list(input: &Value, field: &str) -> Result<Vec<String>, String> {
    let Some(v) = input.get(field) else {
        return Ok(Vec::new());
    };
    let Some(items) = v.as_array() else {
        return Err(format!("`{field}` must be an array of strings."));
    };
    items
        .iter()
        .map(|i| {
            i.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("`{field}` must be an array of strings."))
        })
        .collect()
}

impl Tool for ReportResultTool {
    fn name(&self) -> &'static str {
        "report_result"
    }
    fn description(&self) -> &str {
        "Record your final, typed result and stop. This is how your work reaches whoever \
         delegated it — prose after this call is not read. Call it exactly once, last: say \
         what you found or did in `summary`, name the files you touched and any commits you \
         made, and cite `path:line` for each claim a reader would want to check. Use \
         `outcome: completed` only when the task is actually done; `blocked` when something \
         stopped you; `needs_input` (with `question`) when you need a human decision; \
         `unverifiable` when you did the work but could not confirm it."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "outcome": {
                    "type": "string",
                    "enum": OUTCOMES,
                    "description": "completed | blocked | needs_input | unverifiable."
                },
                "summary": {
                    "type": "string",
                    "description": format!(
                        "What you found or did, at most {MAX_SUMMARY_CHARS} characters."
                    )
                },
                "files_touched": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Every path you created, edited or deleted."
                },
                "commits": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Commit shas you made, if any."
                },
                "citations": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "`path:line` for each claim worth checking."
                },
                "question": {
                    "type": "string",
                    "description": "Required with `needs_input`: the one question a human must answer."
                }
            },
            "required": ["outcome", "summary"]
        })
    }
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    /// It writes one file inside the child's own scratch dir, never the
    /// workspace — a `ToolScope::ReadOnly` child must still be able to report.
    fn read_only(&self) -> bool {
        true
    }
    fn display_summary(&self, input: &Value) -> Option<String> {
        Some(format!(
            "report_result ({})",
            input.get("outcome").and_then(Value::as_str).unwrap_or("?")
        ))
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let Some(outcome) = input.get("outcome").and_then(Value::as_str) else {
                return ToolOutcome::err(format!(
                    "`outcome` is required and must be one of: {}.",
                    OUTCOMES.join(", ")
                ));
            };
            if !OUTCOMES.contains(&outcome) {
                return ToolOutcome::err(format!(
                    "`outcome` was `{outcome}`, which is not one of: {}. Pick the one that \
                     describes what actually happened and call report_result again.",
                    OUTCOMES.join(", ")
                ));
            }
            let Some(summary) = input.get("summary").and_then(Value::as_str) else {
                return ToolOutcome::err("`summary` is required: what you found or did.");
            };
            let len = summary.chars().count();
            if len > MAX_SUMMARY_CHARS {
                return ToolOutcome::err(format!(
                    "`summary` is {len} characters against a cap of {MAX_SUMMARY_CHARS}. \
                     Cut it to the findings the caller has to act on and call report_result \
                     again — nothing was recorded."
                ));
            }
            let question = input.get("question").and_then(Value::as_str);
            if outcome == "needs_input" && question.is_none() {
                return ToolOutcome::err(
                    "`needs_input` needs a `question`: the one thing a human must decide. \
                     Add it and call report_result again.",
                );
            }
            let lists = ["files_touched", "commits", "citations"]
                .into_iter()
                .map(|f| string_list(&input, f).map(|v| (f, v)))
                .collect::<Result<Vec<_>, String>>();
            let lists = match lists {
                Ok(l) => l,
                Err(e) => return ToolOutcome::err(e),
            };
            let mut value = json!({"outcome": outcome, "summary": summary});
            for (field, items) in lists {
                value[field] = json!(items);
            }
            if let Some(q) = question {
                value["question"] = json!(q);
            }

            if let Some(parent) = self.path.parent() {
                if let Err(e) = hotl_platform::PRIVATE_FS.create_dir_all(parent) {
                    return ToolOutcome::err(format!("Could not record the result: {e}"));
                }
            }
            let write = hotl_platform::PRIVATE_FS
                .create_file_truncate(&self.path)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(
                        serde_json::to_string_pretty(&value)
                            .unwrap_or_default()
                            .as_bytes(),
                    )
                });
            match write {
                Ok(()) => ToolOutcome::ok("Recorded. Stop now."),
                Err(e) => ToolOutcome::err(format!("Could not record the result: {e}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(dir: &Path) -> ReportResultTool {
        ReportResultTool::new(dir.join(RESPONSE_FILE))
    }

    #[tokio::test]
    async fn a_valid_report_lands_on_disk_and_tells_the_child_to_stop() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path());
        let out = t
            .run(
                json!({
                    "outcome": "completed",
                    "summary": "found it",
                    "files_touched": ["a.rs"],
                    "citations": ["a.rs:12"]
                }),
                CancellationToken::new(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "Recorded. Stop now.");
        let v = read_response(&dir.path().join(RESPONSE_FILE)).expect("response written");
        assert_eq!(v["outcome"], "completed");
        assert_eq!(v["files_touched"][0], "a.rs");
        assert_eq!(v["commits"], json!([]));
    }

    #[tokio::test]
    async fn an_oversized_summary_is_refused_with_the_numbers_named() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_SUMMARY_CHARS + 1);
        let out = tool(dir.path())
            .run(
                json!({"outcome": "completed", "summary": big}),
                CancellationToken::new(),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains(&(MAX_SUMMARY_CHARS + 1).to_string()));
        assert!(out.content.contains(&MAX_SUMMARY_CHARS.to_string()));
        assert!(
            !dir.path().join(RESPONSE_FILE).exists(),
            "a refused report must record nothing"
        );
    }

    #[tokio::test]
    async fn an_unknown_outcome_and_a_questionless_needs_input_are_both_refused() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path());
        let out = t
            .run(
                json!({"outcome": "vibes", "summary": "s"}),
                CancellationToken::new(),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("unverifiable"), "{}", out.content);

        let out = t
            .run(
                json!({"outcome": "needs_input", "summary": "s"}),
                CancellationToken::new(),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("question"), "{}", out.content);
    }

    /// A read-only child (`explore`, `plan`) is the fan-out hot path and must
    /// still be able to answer — the tool writes only its own scratch file.
    #[test]
    fn the_tool_is_read_only_and_gate_free() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path());
        assert!(t.read_only());
        assert!(matches!(t.permission(&json!({})), Permission::None));
        assert!(!t.edits_files());
    }
}
