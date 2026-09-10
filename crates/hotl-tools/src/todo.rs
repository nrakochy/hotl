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
pub use hotl_types::{Decision, Todo, TodoStatus};

/// The tool doesn't hold the list itself — it forwards the validated list to
/// the actor (the single owner) via this sink and returns a confirmation the
/// model reads. A plain `Fn`, not an async channel send, so hotl-tools never
/// depends on hotl-engine: the binary supplies a closure that reaches the
/// session's actor (mirrors how the `spawn` tool's `ChildBuilder` decouples
/// hotl-tools from the engine crate it ultimately talks to).
type Sink = Arc<dyn Fn(Vec<Todo>, Vec<Decision>) + Send + Sync>;

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
/// the human oriented), unique ids, resolvable dependencies, no cycles. An
/// empty list is allowed through — that's the model signaling "done", not a
/// malformed call.
///
/// Ids are assigned `n1..nN` positionally when absent and kept when present,
/// which is what lets `dependencies` survive the model rewriting the whole
/// list every call.
pub fn parse_todos(input: &Value) -> Result<Vec<Todo>, ToolOutcome> {
    let arr = input
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ToolOutcome::err(
                "`todos` must be an array of {content, status, active_form?, id?, \
                 dependencies?, acceptance?, validate_cmd?, replan?}. Re-send it.",
            )
        })?;
    let mut out = Vec::with_capacity(arr.len());
    let mut in_progress = 0;
    for (i, v) in arr.iter().enumerate() {
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
        let id = v
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| format!("n{}", i + 1), str::to_string);
        let dependencies: Vec<String> = v
            .get("dependencies")
            .and_then(Value::as_array)
            .map(|d| {
                d.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        out.push(Todo {
            content,
            status,
            active_form: v
                .get("active_form")
                .and_then(Value::as_str)
                .map(str::to_string),
            id: Some(id),
            dependencies,
            acceptance: str_field(v, "acceptance"),
            validate_cmd: str_field(v, "validate_cmd"),
            replan: v.get("replan").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    if in_progress > 1 {
        return Err(ToolOutcome::err(
            "At most one todo may be `in_progress` — mark just the item you're working on now.",
        ));
    }
    check_dependencies(&out)?;
    Ok(out)
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Ids unique, every dependency resolves, no cycles. The errors name the
/// offending id and what to do about it — a "bad dependencies" refusal the
/// model cannot act on just gets re-sent unchanged.
fn check_dependencies(items: &[Todo]) -> Result<(), ToolOutcome> {
    let ids: Vec<&str> = items
        .iter()
        .map(|t| t.id.as_deref().unwrap_or_default())
        .collect();
    for (i, id) in ids.iter().enumerate() {
        if ids[..i].contains(id) {
            return Err(ToolOutcome::err(format!(
                "Two steps share the id `{id}`. Ids must be unique — give one of them a \
                 different id, or drop `id` and let them be numbered."
            )));
        }
    }
    for t in items {
        for dep in &t.dependencies {
            if !ids.contains(&dep.as_str()) {
                let me = t.id.as_deref().unwrap_or("?");
                return Err(ToolOutcome::err(format!(
                    "Step `{me}` depends on `{dep}`, which is not a step in this list. Use \
                     the id of a step you sent, or drop the dependency."
                )));
            }
        }
    }
    // Iterative DFS with a three-colour mark, so a long chain cannot blow the
    // stack the way a recursive walk would.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        White,
        Grey,
        Black,
    }
    let mut mark = vec![Mark::White; items.len()];
    let index = |id: &str| ids.iter().position(|x| *x == id);
    for start in 0..items.len() {
        if mark[start] != Mark::White {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        mark[start] = Mark::Grey;
        while let Some((node, next)) = stack.pop() {
            match items[node].dependencies.get(next) {
                Some(dep) => {
                    stack.push((node, next + 1));
                    let Some(d) = index(dep) else { continue };
                    match mark[d] {
                        Mark::Grey => {
                            let cycle: Vec<&str> = stack
                                .iter()
                                .map(|(n, _)| ids[*n])
                                .chain(std::iter::once(ids[d]))
                                .collect();
                            return Err(ToolOutcome::err(format!(
                                "The dependencies form a cycle: {}. Break it — a step cannot \
                                 wait on its own output.",
                                cycle.join(" → ")
                            )));
                        }
                        Mark::White => {
                            mark[d] = Mark::Grey;
                            stack.push((d, 0));
                        }
                        Mark::Black => {}
                    }
                }
                None => mark[node] = Mark::Black,
            }
        }
    }
    Ok(())
}

/// The tagged reminder injected into context. `None` when the list is empty.
/// This is the model-facing render — an `Item::User` tagged
/// `SyntheticReason::Todos`, never committed to the durable log (it rides
/// the snapshot the turn samples against, like the MOIM turn-context block).
pub fn render_reminder(items: &[Todo]) -> Option<Item> {
    if items.is_empty() {
        return None;
    }
    let failed = items
        .iter()
        .filter(|t| t.status == TodoStatus::Failed)
        .count();
    // The attribute only appears when something failed: a list with no
    // failures renders exactly the bytes it did before 0056, so an ordinary
    // session's cached prefix is untouched.
    let mut body = if failed > 0 {
        format!("<todos failed=\"{failed}\">\n")
    } else {
        String::from("<todos>\n")
    };
    for t in items {
        let mark = match t.status {
            TodoStatus::Completed => "[x]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Failed => "[!]",
            TodoStatus::NeedsMoreSteps => "[?]",
            _ => "[ ]",
        };
        body.push_str(mark);
        body.push(' ');
        body.push_str(&t.content);
        if let Some(cmd) = &t.validate_cmd {
            body.push_str(&format!("  → verify: {cmd}"));
        }
        if !t.dependencies.is_empty() {
            body.push_str(&format!("  ⇐ after {}", t.dependencies.join(", ")));
        }
        body.push('\n');
    }
    body.push_str("</todos>");
    Some(Item::User {
        text: body,
        synthetic: Some(SyntheticReason::Todos),
        images: Vec::new(),
    })
}

fn summary(items: &[Todo], decisions: usize) -> String {
    let c = |s| items.iter().filter(|t| t.status == s).count();
    let mut s = format!(
        "Todos updated: {} in progress, {} pending, {} done",
        c(TodoStatus::InProgress),
        c(TodoStatus::Pending),
        c(TodoStatus::Completed)
    );
    let failed = c(TodoStatus::Failed);
    if failed > 0 {
        s.push_str(&format!(", {failed} failed"));
    }
    if decisions > 0 {
        s.push_str(&format!(" · {decisions} decision(s) recorded"));
    }
    s
}

/// The decisions half of a `todo_write` call. Malformed entries are dropped
/// rather than refused: the nodes are the load-bearing half, and failing the
/// whole call over a missing `why` would cost the plan update too. `when_ms`
/// is zero here — the actor stamps it.
fn parse_decisions(input: &Value) -> Vec<Decision> {
    input
        .get("decisions")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let what = str_field(v, "what")?;
                    let why = str_field(v, "why")?;
                    Some(Decision {
                        when_ms: 0,
                        what,
                        why,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Tool for TodoWriteTool {
    fn name(&self) -> &'static str {
        "todo_write"
    }
    fn description(&self) -> &str {
        "Maintain your task list for this session. Use it proactively for any work with \
         more than one step, and update it as you go rather than at the end — the user \
         watches it to see progress. Send the ENTIRE list each time (it replaces the \
         previous one). Mark exactly one item `in_progress` before starting it and \
         `completed` the moment it is done (never batch completions); add follow-up work \
         you discover as new items. Only a single trivial action needs no list. \
         Draft steps first, then set `dependencies`: a step that needs another's output \
         depends on it; steps with no dependency run in any order. Give every step whose \
         result can be checked a `validate_cmd` (the exact test/build command) or an \
         `acceptance` sentence. One step = one uninterrupted stretch of work; do not split \
         a step you would never pause between."
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
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "failed", "needs_more_steps"],
                                "description": "`failed` = tried and did not work; `needs_more_steps` = bigger than one step, split it"
                            },
                            "active_form": {
                                "type": "string",
                                "description": "present-tense label shown while in progress"
                            },
                            "id": {
                                "type": "string",
                                "description": "stable id (n1, n2, …); assigned for you when omitted — send it back unchanged so dependencies survive a rewrite"
                            },
                            "dependencies": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "ids of steps whose output this one needs"
                            },
                            "acceptance": {
                                "type": "string",
                                "description": "one sentence for what done looks like, when no command can say"
                            },
                            "validate_cmd": {
                                "type": "string",
                                "description": "the exact command that proves this step (e.g. `cargo nextest run -p fx`)"
                            },
                            "replan": {
                                "type": "boolean",
                                "description": "this step's result is expected to change the rest of the plan"
                            }
                        },
                        "required": ["content", "status"]
                    }
                },
                "decisions": {
                    "type": "array",
                    "description": "choices worth remembering, APPENDED (not replaced) to the plan's decisions log — record one when you pick an approach a later reader would otherwise have to re-derive",
                    "items": {
                        "type": "object",
                        "properties": {
                            "what": {"type": "string", "description": "the choice, in a few words"},
                            "why": {"type": "string", "description": "the reason it beat the alternative"}
                        },
                        "required": ["what", "why"]
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
                    let decisions = parse_decisions(&input);
                    let s = summary(&items, decisions.len());
                    (self.sink)(items, decisions);
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
    fn parse_assigns_ids_and_rejects_unknown_dependencies() {
        let ok = parse_todos(&json!({"todos":[
            {"content":"a","status":"pending"},
            {"content":"b","status":"pending","dependencies":["n1"],
             "validate_cmd":"cargo test -p x","acceptance":"x builds","replan":true}
        ]}))
        .unwrap();
        assert_eq!(ok[0].id.as_deref(), Some("n1"));
        assert_eq!(ok[1].id.as_deref(), Some("n2"));
        assert_eq!(ok[1].dependencies, vec!["n1".to_string()]);
        assert_eq!(ok[1].validate_cmd.as_deref(), Some("cargo test -p x"));
        assert!(ok[1].replan);

        // An id the model sent is kept verbatim — that is what makes a
        // dependency survive the next full-state rewrite.
        let kept = parse_todos(&json!({"todos":[
            {"content":"a","status":"pending","id":"setup"},
            {"content":"b","status":"pending","dependencies":["setup"]}
        ]}))
        .unwrap();
        assert_eq!(kept[0].id.as_deref(), Some("setup"));

        let err = parse_todos(&json!({"todos":[
            {"content":"a","status":"pending"},
            {"content":"b","status":"pending","dependencies":["n9"]}
        ]}))
        .unwrap_err();
        assert!(err.is_error);
        assert!(err.content.contains("n9"), "{}", err.content);

        let dup = parse_todos(&json!({"todos":[
            {"content":"a","status":"pending","id":"x"},
            {"content":"b","status":"pending","id":"x"}
        ]}))
        .unwrap_err();
        assert!(dup.content.contains('x'), "{}", dup.content);
    }

    #[test]
    fn parse_rejects_dependency_cycles() {
        let err = parse_todos(&json!({"todos":[
            {"content":"a","status":"pending","dependencies":["n2"]},
            {"content":"b","status":"pending","dependencies":["n1"]}
        ]}))
        .unwrap_err();
        assert!(err.is_error);
        assert!(err.content.contains("cycle"), "{}", err.content);
        // A self-edge is the degenerate cycle and must not slip past.
        let selfdep = parse_todos(&json!({"todos":[
            {"content":"a","status":"pending","dependencies":["n1"]}
        ]}))
        .unwrap_err();
        assert!(selfdep.content.contains("cycle"), "{}", selfdep.content);
        // A diamond is not a cycle: n4 ← n2,n3 ← n1 must parse.
        let diamond = parse_todos(&json!({"todos":[
            {"content":"a","status":"pending"},
            {"content":"b","status":"pending","dependencies":["n1"]},
            {"content":"c","status":"pending","dependencies":["n1"]},
            {"content":"d","status":"pending","dependencies":["n2","n3"]}
        ]}));
        assert!(diamond.is_ok(), "a diamond is not a cycle");
    }

    #[test]
    fn render_shows_marks_verify_and_after() {
        let items = parse_todos(&json!({"todos":[
            {"content":"build","status":"completed"},
            {"content":"test","status":"failed","validate_cmd":"cargo test -p x",
             "dependencies":["n1"]},
            {"content":"ship","status":"needs_more_steps"}
        ]}))
        .unwrap();
        let Item::User { text, .. } = render_reminder(&items).unwrap() else {
            panic!("todo reminder must be a user item");
        };
        assert!(text.starts_with("<todos failed=\"1\">"), "{text}");
        assert!(text.contains("[x] build"), "{text}");
        assert!(text.contains("[!] test"), "{text}");
        assert!(text.contains("[?] ship"), "{text}");
        assert!(text.contains("→ verify: cargo test -p x"), "{text}");
        assert!(text.contains("⇐ after n1"), "{text}");
    }

    #[test]
    fn render_reminder_none_when_empty_and_tagged_otherwise() {
        assert!(render_reminder(&[]).is_none());
        let item = render_reminder(&[Todo {
            content: "a".into(),
            status: TodoStatus::Pending,
            ..Todo::default()
        }])
        .unwrap();
        match item {
            hotl_types::Item::User {
                text, synthetic, ..
            } => {
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
        let tool = TodoWriteTool::new(Arc::new(move |items, _d| *s.lock().unwrap() = items));
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
        let tool = TodoWriteTool::new(Arc::new(move |items, _d| *s.lock().unwrap() = items));
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
