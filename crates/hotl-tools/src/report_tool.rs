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

/// [`unverifiable`] naming what ended the child (`idle`, …), so the parent
/// model can tell a stuck sub-agent from one that simply never reported.
pub fn unverifiable_because(reason: &str, summary: &str) -> Value {
    json!({"outcome": "unverifiable", "reason": reason, "summary": summary})
}

/// The brief's "## Return" section. A native child reports through the
/// tool; an external (ACP) one has no tool, so it ends on a fenced block.
pub fn return_section(external: bool) -> String {
    match external {
        false => format!(
            "## Return\n\nEnd by calling `report_result` exactly once with \
             `outcome` (completed | blocked | needs_input | unverifiable), a `summary` of at most \
             {MAX_SUMMARY_CHARS} characters, `files_touched`, `commits` and `citations` \
             (`path:line`). Prose after that call is not read.\n"
        ),
        true => format!(
            "## Return\n\nEnd your final message with one fenced ```json block containing \
             exactly: `outcome` (completed | blocked | needs_input | unverifiable), `summary` (at \
             most {MAX_SUMMARY_CHARS} characters), `files_touched`, `commits` and `citations` \
             (`path:line`); add `question` with needs_input. Text after the block is not read.\n"
        ),
    }
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

/// The checks `report_result` runs, as a value: the normalized result, or
/// the message the tool would have returned. Shared with an external
/// agent's fenced report (0077 D4) so both answers mean the same thing.
pub fn validate(input: &Value) -> Result<Value, String> {
    let Some(outcome) = input.get("outcome").and_then(Value::as_str) else {
        return Err(format!(
            "`outcome` is required and must be one of: {}.",
            OUTCOMES.join(", ")
        ));
    };
    if !OUTCOMES.contains(&outcome) {
        return Err(format!(
            "`outcome` was `{outcome}`, which is not one of: {}. Pick the one that \
             describes what actually happened and call report_result again.",
            OUTCOMES.join(", ")
        ));
    }
    let Some(summary) = input.get("summary").and_then(Value::as_str) else {
        return Err("`summary` is required: what you found or did.".into());
    };
    let len = summary.chars().count();
    if len > MAX_SUMMARY_CHARS {
        return Err(format!(
            "`summary` is {len} characters against a cap of {MAX_SUMMARY_CHARS}. \
             Cut it to the findings the caller has to act on and call report_result \
             again — nothing was recorded."
        ));
    }
    let question = input.get("question").and_then(Value::as_str);
    if outcome == "needs_input" && question.is_none() {
        return Err(
            "`needs_input` needs a `question`: the one thing a human must decide. \
             Add it and call report_result again."
                .into(),
        );
    }
    let lists = ["files_touched", "commits", "citations"]
        .into_iter()
        .map(|f| string_list(input, f).map(|v| (f, v)))
        .collect::<Result<Vec<_>, String>>()?;
    let mut value = json!({"outcome": outcome, "summary": summary});
    for (field, items) in lists {
        value[field] = json!(items);
    }
    if let Some(q) = question {
        value["question"] = json!(q);
    }
    Ok(value)
}

/// The **last** fenced ```` ```json ```` block in `text` (or, with no fence,
/// a trailing line that is a bare `{…}` object), validated. `None` when there
/// is no candidate at all.
pub fn extract_fenced_report(text: &str) -> Option<Result<Value, String>> {
    let candidate = last_json_fence(text).or_else(|| {
        let line = text.trim_end().lines().last()?.trim();
        (line.starts_with('{') && line.ends_with('}')).then(|| line.to_string())
    })?;
    Some(match serde_json::from_str::<Value>(&candidate) {
        Ok(v) if v.is_object() => validate(&v),
        Ok(_) => Err("the fenced block is not a JSON object.".into()),
        Err(e) => Err(format!("the fenced block is not valid JSON: {e}.")),
    })
}

fn last_json_fence(text: &str) -> Option<String> {
    let mut found = None;
    let mut open: Option<Vec<&str>> = None;
    for line in text.lines() {
        let t = line.trim();
        match &mut open {
            None if t.starts_with("```json") || t == "```JSON" => open = Some(Vec::new()),
            Some(body) if t == "```" => {
                found = Some(body.join("\n"));
                open = None;
            }
            Some(body) => body.push(line),
            None => {}
        }
    }
    found
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
            let value = match validate(&input) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::err(e),
            };
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

    /// The factored validator and the tool agree on every input, error
    /// strings included.
    #[tokio::test]
    async fn validate_matches_the_tool_byte_for_byte() {
        let cases = [
            json!({"summary": "s"}),
            json!({"outcome": "vibes", "summary": "s"}),
            json!({"outcome": "completed"}),
            json!({"outcome": "completed", "summary": "x".repeat(MAX_SUMMARY_CHARS + 1)}),
            json!({"outcome": "needs_input", "summary": "s"}),
            json!({"outcome": "completed", "summary": "s", "commits": "abc"}),
            json!({"outcome": "completed", "summary": "s", "citations": [1]}),
        ];
        for input in cases {
            let dir = tempfile::tempdir().unwrap();
            let out = tool(dir.path())
                .run(input.clone(), CancellationToken::new())
                .await;
            assert!(out.is_error);
            assert_eq!(validate(&input).unwrap_err(), out.content, "{input}");
        }
        let ok = json!({"outcome": "needs_input", "summary": "s", "question": "q?", "files_touched": ["a"]});
        let dir = tempfile::tempdir().unwrap();
        let out = tool(dir.path())
            .run(ok.clone(), CancellationToken::new())
            .await;
        assert!(!out.is_error);
        assert_eq!(
            read_response(&dir.path().join(RESPONSE_FILE)).unwrap(),
            validate(&ok).unwrap()
        );
    }

    #[test]
    fn the_last_fenced_json_block_wins() {
        let text = "first:\n```json\n{\"outcome\": \"blocked\", \"summary\": \"old\"}\n```\nthen:\n```json\n{\"outcome\": \"completed\", \"summary\": \"new\", \"citations\": [\"a.rs:1\"]}\n```\ntrailing prose";
        let v = extract_fenced_report(text).unwrap().unwrap();
        assert_eq!(v["summary"], "new");
        assert_eq!(v["commits"], json!([]));
        assert!(extract_fenced_report("no report here").is_none());
    }

    #[test]
    fn an_unfenced_trailing_object_is_accepted() {
        let text = "done.\n{\"outcome\": \"completed\", \"summary\": \"ok\"}\n";
        let v = extract_fenced_report(text).unwrap().unwrap();
        assert_eq!(v["outcome"], "completed");
    }

    #[test]
    fn an_invalid_block_reports_the_validation_error() {
        let text = "```json\n{\"outcome\": \"vibes\", \"summary\": \"s\"}\n```";
        let err = extract_fenced_report(text).unwrap().unwrap_err();
        assert_eq!(
            err,
            validate(&json!({"outcome": "vibes", "summary": "s"})).unwrap_err()
        );
        let err = extract_fenced_report("```json\n{not json\n```")
            .unwrap()
            .unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
    }

    #[test]
    fn the_return_sections_differ_only_in_how_to_return() {
        assert!(return_section(false).contains("`report_result`"));
        let ext = return_section(true);
        assert!(
            ext.contains("```json") && !ext.contains("report_result"),
            "{ext}"
        );
    }
}
