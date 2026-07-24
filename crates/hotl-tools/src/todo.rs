//! `todo_write` — full-state session checklist. The model rewrites the whole
//! list each call (idempotent); the actor owns it and the TodoGate reads it.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use hotl_types::{Item, SyntheticReason};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{Permission, Tool, ToolOutcome};

// Re-exported so tools/engine/surfaces share one definition (hotl-types is
// the canonical home; this module is where the tool-facing API lives).
pub use hotl_types::{Todo, TodoStatus};

/// The tool doesn't hold the list itself — it forwards the validated list to
/// the actor (the single owner) via this sink and returns a confirmation the
/// model reads. A plain `Fn`, not an async channel send, so hotl-tools never
/// depends on hotl-engine: the binary supplies a closure that reaches the
/// session's actor (mirrors how the `spawn` tool's `ChildBuilder` decouples
/// hotl-tools from the engine crate it ultimately talks to).
type Sink = Arc<dyn Fn(Vec<Todo>) + Send + Sync>;

pub struct TodoWriteTool {
    sink: Sink,
}

impl TodoWriteTool {
    pub fn new(sink: Sink) -> Self {
        Self { sink }
    }
}

/// Full-state parse + validation: non-empty content on every item, at most
/// one `in_progress` (the corpus convention — exactly one active item keeps
/// the human oriented). An empty list is allowed through — that's the model
/// signaling "done", not a malformed call.
pub fn parse_todos(input: &Value) -> Result<Vec<Todo>, ToolOutcome> {
    let arr = input
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ToolOutcome::err(
                "`todos` must be an array of {content, status, active_form?}. Re-send it.",
            )
        })?;
    let mut out = Vec::with_capacity(arr.len());
    let mut in_progress = 0;
    for v in arr {
        let content = v
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() {
            return Err(ToolOutcome::err("Every todo needs non-empty `content`."));
        }
        let status: TodoStatus =
            serde_json::from_value(v.get("status").cloned().unwrap_or(json!("pending")))
                .unwrap_or(TodoStatus::Pending);
        if status == TodoStatus::InProgress {
            in_progress += 1;
        }
        out.push(Todo {
            content,
            status,
            active_form: v
                .get("active_form")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    if in_progress > 1 {
        return Err(ToolOutcome::err(
            "At most one todo may be `in_progress` — mark just the item you're working on now.",
        ));
    }
    Ok(out)
}

/// The tagged reminder injected into context. `None` when the list is empty.
/// This is the model-facing render — an `Item::User` tagged
/// `SyntheticReason::Todos`, never committed to the durable log (it rides
/// the snapshot the turn samples against, like the MOIM turn-context block).
pub fn render_reminder(items: &[Todo]) -> Option<Item> {
    if items.is_empty() {
        return None;
    }
    let mut body = String::from("<todos>\n");
    for t in items {
        let mark = match t.status {
            TodoStatus::Completed => "[x]",
            TodoStatus::InProgress => "[~]",
            _ => "[ ]",
        };
        body.push_str(&format!("{mark} {}\n", t.content));
    }
    body.push_str("</todos>");
    Some(Item::User {
        text: body,
        synthetic: Some(SyntheticReason::Todos),
    })
}

fn summary(items: &[Todo]) -> String {
    let c = |s| items.iter().filter(|t| t.status == s).count();
    format!(
        "Todos updated: {} in progress, {} pending, {} done",
        c(TodoStatus::InProgress),
        c(TodoStatus::Pending),
        c(TodoStatus::Completed)
    )
}

impl Tool for TodoWriteTool {
    fn name(&self) -> &'static str {
        "todo_write"
    }
    fn description(&self) -> &str {
        "Record or update your task list for this session. Send the ENTIRE list each time \
         (it replaces the previous one). Mark exactly one item `in_progress` while you work on it, \
         `completed` the moment it's done, `pending` for not-started. Use it for multi-step work so \
         you and the user can see progress; skip it for single trivial actions."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                            "active_form": {
                                "type": "string",
                                "description": "present-tense label shown while in progress"
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    // Default `read_only` (false) and `parallel_safe` (false) are correct:
    // it mutates shared session state, so calls stay serial.
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            match parse_todos(&input) {
                Ok(items) => {
                    let s = summary(&items);
                    (self.sink)(items);
                    ToolOutcome::ok(s)
                }
                Err(e) => e,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn parse_rejects_empty_content_and_multiple_in_progress() {
        assert!(parse_todos(&json!({"todos":[{"content":"","status":"pending"}]})).is_err());
        assert!(parse_todos(&json!({"todos":[
            {"content":"a","status":"in_progress"},
            {"content":"b","status":"in_progress"}
        ]}))
        .is_err());
        let ok = parse_todos(&json!({"todos":[
            {"content":"a","status":"in_progress","active_form":"doing a"},
            {"content":"b","status":"pending"}
        ]}))
        .unwrap();
        assert_eq!(ok.len(), 2);
    }

    #[test]
    fn parse_allows_clearing_the_list() {
        let ok = parse_todos(&json!({"todos": []})).unwrap();
        assert!(ok.is_empty());
    }

    #[test]
    fn render_reminder_none_when_empty_and_tagged_otherwise() {
        assert!(render_reminder(&[]).is_none());
        let item = render_reminder(&[Todo {
            content: "a".into(),
            status: TodoStatus::Pending,
            active_form: None,
        }])
        .unwrap();
        match item {
            hotl_types::Item::User { text, synthetic } => {
                assert_eq!(synthetic, Some(hotl_types::SyntheticReason::Todos));
                assert!(text.contains("<todos") && text.contains("[ ] a"));
            }
            _ => panic!("todo reminder must be a tagged user item"),
        }
    }

    #[tokio::test]
    async fn tool_forwards_and_confirms() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let tool = TodoWriteTool::new(Arc::new(move |items| *s.lock().unwrap() = items));
        let out = tool
            .run(
                json!({"todos":[{"content":"x","status":"completed"}]}),
                CancellationToken::new(),
            )
            .await;
        assert!(!out.is_error && out.content.contains("1 done"));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn tool_reports_validation_errors_without_forwarding() {
        let seen = Arc::new(Mutex::new(Vec::<Todo>::new()));
        let s = seen.clone();
        let tool = TodoWriteTool::new(Arc::new(move |items| *s.lock().unwrap() = items));
        let out = tool
            .run(
                json!({"todos":[{"content":"","status":"pending"}]}),
                CancellationToken::new(),
            )
            .await;
        assert!(out.is_error);
        assert!(seen.lock().unwrap().is_empty());
    }
}
