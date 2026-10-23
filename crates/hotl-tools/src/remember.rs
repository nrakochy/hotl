//! `remember` (0069): the model proposes one note for the project's memory
//! file. The gate's confirm *is* the owner's yes (`AskProtected { HotlOwn }`,
//! so `bypass` refuses loudly instead of auto-allowing); the tool itself never
//! touches a file — the sink reaches the session actor, the only writer.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{Permission, ProtectedClass, Tool, ToolOutcome};

/// Hands a normalized note to the actor and resolves when the write landed
/// (`Ok`) or was refused (`Err(reason)`). Built by hotl-engine; hotl-tools
/// cannot see the engine (the `QuestionSink` precedent).
pub type MemorySink = Arc<dyn Fn(String) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

pub struct RememberTool {
    sink: MemorySink,
}

impl RememberTool {
    pub fn new(sink: MemorySink) -> Self {
        Self { sink }
    }
}

fn note_of(input: &Value) -> Option<String> {
    input
        .get("note")
        .and_then(Value::as_str)
        .and_then(hotl_types::normalize_memory_note)
}

impl Tool for RememberTool {
    fn name(&self) -> &'static str {
        "remember"
    }
    fn description(&self) -> &str {
        "Propose one short note to keep in this project's memory for future sessions — a fact \
         about the repo, a convention, a decision. The owner confirms before anything is \
         written; nothing is written if they decline. Not for task state (use todo_write) or \
         secrets."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note": {"type": "string", "description": "One line, up to 2000 characters"}
            },
            "required": ["note"]
        })
    }
    fn permission(&self, input: &Value) -> Permission {
        let note = note_of(input).unwrap_or_default();
        Permission::AskProtected {
            summary: format!("remember: \"{note}\""),
            why: "writes your project memory file (data dir); loads into every future session \
                  here"
                .into(),
            class: ProtectedClass::HotlOwn,
        }
    }
    fn display_summary(&self, _input: &Value) -> Option<String> {
        Some("remember".into())
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let Some(note) = note_of(&input) else {
                return ToolOutcome::err(format!(
                    "`note` must be one line of 1–{} characters.",
                    hotl_types::MEMORY_NOTE_MAX_CHARS
                ));
            };
            match (self.sink)(note).await {
                Ok(()) => ToolOutcome::ok("saved to project memory — loads next session"),
                Err(why) => ToolOutcome::err(format!("not saved: {why}")),
            }
        })
    }
}
