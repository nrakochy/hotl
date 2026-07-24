//! `ask_user` — a structured multiple-choice question to the human (tier-1
//! gap #4). Unlike the permission `ask`, this is a *data-gathering*
//! round-trip: the answer is a plain-text tool result the model reads, never
//! a permission grant. SECURITY invariant: a model cannot launder a
//! mutating action through a question — nothing here authorizes a tool.
//!
//! The tool doesn't reach the engine directly (that would make hotl-tools
//! depend on hotl-engine, a layering cycle — see `todo.rs`'s `Sink` for the
//! established shape). Instead it holds a [`QuestionSink`]: an async
//! closure the binary supplies, bridging to the engine's ask-question event.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{Permission, Tool, ToolOutcome};

// Re-exported so tools/engine/surfaces share one definition (hotl-types is
// the canonical home; this module is where the tool-facing API lives) —
// same shape as `todo.rs`'s re-export of `Todo`/`TodoStatus`.
pub use hotl_types::{Question, QuestionOption};

/// A human's answer to an `ask_user` question. Lives in hotl-types (not
/// hotl-engine) so both this crate's `QuestionSink` and hotl-engine's
/// `EngineEvent::Question` can share it without either crate depending on
/// the other — the plan's own open question ("keep it where `ask_user` can
/// import without a cycle"), resolved the same way `Question` already is.
pub use hotl_types::QuestionAnswer;

/// At most this many options: keeps the picker legible (the
/// `AskUserQuestion` convention this tool matches).
const MAX_OPTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;

/// The tool's bridge to the engine: given a validated `Question` and the
/// call's cancellation token (identical to the token `Turn` races a
/// permission ask against), resolve to the human's answer. A plain async
/// closure, not a channel send, keeps hotl-tools free of any hotl-engine
/// dependency — mirrors `todo.rs::Sink`, widened for the async round-trip
/// and the tool's own per-call cancellation.
pub type QuestionSink =
    Arc<dyn Fn(Question, CancellationToken) -> BoxFuture<'static, QuestionAnswer> + Send + Sync>;

pub struct AskUserTool {
    sink: QuestionSink,
}

impl AskUserTool {
    pub fn new(sink: QuestionSink) -> Self {
        Self { sink }
    }
}

/// Validate + parse one question: a header, a prompt, and 2-4 options each
/// with a non-empty label. 0/1 options should use the permission ask
/// instead (a real y/n); more than 4 keeps the picker legible.
pub fn parse_question(input: &Value) -> Result<Question, ToolOutcome> {
    let header = input
        .get("header")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if header.is_empty() {
        return Err(ToolOutcome::err(
            "`header` is required: a short (≤40 char) label for the question.",
        ));
    }
    let prompt = input
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if prompt.is_empty() {
        return Err(ToolOutcome::err(
            "`prompt` is required: the question to put to the user.",
        ));
    }
    let arr = input
        .get("options")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ToolOutcome::err(format!(
                "`options` must be an array of {{label, description?}}, {MIN_OPTIONS}-{MAX_OPTIONS} entries."
            ))
        })?;
    if arr.len() < MIN_OPTIONS {
        return Err(ToolOutcome::err(format!(
            "At least {MIN_OPTIONS} options are required — for a plain yes/no, use the \
             permission ask instead of `ask_user`."
        )));
    }
    if arr.len() > MAX_OPTIONS {
        return Err(ToolOutcome::err(format!(
            "At most {MAX_OPTIONS} options are allowed — keep the choice legible; narrow it down."
        )));
    }
    let mut options = Vec::with_capacity(arr.len());
    for v in arr {
        let label = v
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if label.is_empty() {
            return Err(ToolOutcome::err("Every option needs a non-empty `label`."));
        }
        options.push(QuestionOption {
            label,
            description: v
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        });
    }
    let multi = input.get("multi").and_then(Value::as_bool).unwrap_or(false);
    Ok(Question {
        header,
        prompt,
        options,
        multi,
    })
}

/// The tool result the model reads. `Selected` joins the chosen labels;
/// `FreeText` is prefixed so the model can tell it wasn't one of the listed
/// options; `NoHuman` is the headless/no-human guidance — always resolves,
/// never a dead end, so an unattended run proceeds instead of hanging.
pub fn format_answer(answer: &QuestionAnswer) -> String {
    match answer {
        QuestionAnswer::Selected(labels) => labels.join(", "),
        QuestionAnswer::FreeText(text) => format!("The user answered: {text}"),
        QuestionAnswer::NoHuman => {
            "No human is available to answer; proceed with your best judgment and state the \
             assumption you made."
                .to_string()
        }
    }
}

impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask_user"
    }
    fn description(&self) -> &str {
        "Ask the human a structured multiple-choice question when a genuine ambiguity would \
         otherwise force you to guess. Give a short header, a clear prompt, and 2-4 labelled \
         options (the human can also answer with free text). This does NOT grant permission for \
         any action — it only gathers information; you still need normal permission to act on \
         the answer."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "header": {"type": "string", "description": "short label (≤40 chars), e.g. \"Auth provider\""},
                "prompt": {"type": "string", "description": "the question to ask"},
                "options": {
                    "type": "array",
                    "minItems": MIN_OPTIONS,
                    "maxItems": MAX_OPTIONS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {"type": "string"},
                            "description": {"type": "string"}
                        },
                        "required": ["label"]
                    }
                },
                "multi": {"type": "boolean", "description": "allow selecting more than one option (default false)"}
            },
            "required": ["header", "prompt", "options"]
        })
    }
    fn permission(&self, _input: &Value) -> Permission {
        // Not a permission gate (SECURITY invariant): asking a question
        // changes nothing on disk and authorizes no other tool.
        Permission::None
    }
    // Asking a question has no filesystem/execution/network side effect, so
    // it is read-only — the agent can ask clarifying questions *during* plan
    // mode, which is exactly when it should. Default `parallel_safe` (false)
    // is correct: it has an interactive side effect and its ordering
    // matters, so it always runs alone in its chunk.
    fn read_only(&self) -> bool {
        true
    }
    fn run<'a>(&'a self, input: Value, cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let question = match parse_question(&input) {
                Ok(q) => q,
                Err(e) => return e,
            };
            let answer = (self.sink)(question, cancel).await;
            ToolOutcome::ok(format_answer(&answer))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requires_two_to_four_options() {
        let ok = parse_question(&json!({
            "header":"Scope","prompt":"How far?","options":[
                {"label":"MVP"},{"label":"Full","description":"everything"}
            ]
        }))
        .unwrap();
        assert_eq!(ok.options.len(), 2);
        assert!(
            parse_question(&json!({"header":"x","prompt":"y","options":[{"label":"only"}]}))
                .is_err()
        );
        let five = (0..5)
            .map(|i| json!({"label": format!("o{i}")}))
            .collect::<Vec<_>>();
        assert!(parse_question(&json!({"header":"x","prompt":"y","options":five})).is_err());
    }

    #[test]
    fn parse_requires_header_prompt_and_labels() {
        assert!(
            parse_question(&json!({"prompt":"y","options":[{"label":"a"},{"label":"b"}]})).is_err()
        );
        assert!(
            parse_question(&json!({"header":"x","options":[{"label":"a"},{"label":"b"}]})).is_err()
        );
        assert!(parse_question(
            &json!({"header":"x","prompt":"y","options":[{"label":""},{"label":"b"}]})
        )
        .is_err());
    }

    #[test]
    fn format_answer_shapes_each_variant() {
        assert_eq!(
            format_answer(&QuestionAnswer::Selected(vec!["MVP".into()])),
            "MVP"
        );
        assert_eq!(
            format_answer(&QuestionAnswer::FreeText("do it another way".into())),
            "The user answered: do it another way"
        );
        assert!(format_answer(&QuestionAnswer::NoHuman).contains("No human is available"));
    }

    #[tokio::test]
    async fn tool_forwards_to_sink_and_returns_its_answer() {
        let sink: QuestionSink = Arc::new(|q, _cancel| {
            Box::pin(async move { QuestionAnswer::Selected(vec![q.options[0].label.clone()]) })
        });
        let tool = AskUserTool::new(sink);
        let out = tool
            .run(
                json!({"header":"Scope","prompt":"How far?","options":[{"label":"MVP"},{"label":"Full"}]}),
                CancellationToken::new(),
            )
            .await;
        assert!(!out.is_error);
        assert_eq!(out.content, "MVP");
    }

    #[tokio::test]
    async fn tool_reports_validation_errors_without_calling_the_sink() {
        let sink: QuestionSink = Arc::new(|_q, _cancel| {
            Box::pin(async move { panic!("sink must not be called on a validation error") })
        });
        let tool = AskUserTool::new(sink);
        let out = tool
            .run(
                json!({"header":"x","prompt":"y","options":[]}),
                CancellationToken::new(),
            )
            .await;
        assert!(out.is_error);
    }

    #[test]
    fn ask_user_is_permission_none_and_read_only() {
        let tool = AskUserTool::new(Arc::new(|_q, _c| {
            Box::pin(async { QuestionAnswer::NoHuman })
        }));
        assert_eq!(tool.permission(&json!({})), Permission::None);
        assert!(tool.read_only());
        assert!(!tool.parallel_safe());
        assert_eq!(tool.name(), "ask_user");
    }
}
