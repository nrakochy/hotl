//! The single `EngineEvent` → JSON renderer.
//!
//! There used to be three (`agent.rs::render_json`, `acp.rs::update_payload`,
//! `attach.rs::render_update`) and they had already drifted: the headless one
//! emitted a Rust `Debug` string for `outcome` while ACP emitted a structured
//! tag, and attach handled 4 of 13 variants (evaluation §7). One renderer, one
//! shape, one place to add a variant.
//!
//! Deliberately self-contained — it depends on `hotl-engine` and `serde_json`
//! and nothing else in this crate — so `tests/json_stream_schema.rs` can pull
//! it in by path the way the other integration tests reach `acp.rs`
//! (`crates/hotl` is a binary crate with no lib target).
//!
//! INVARIANT: every `EngineEvent` variant produces a `type`-tagged object, and
//! no payload is a `Debug` rendering. Enforced by
//! `tests/json_stream_schema.rs::every_frame_is_tagged_and_versioned`.

use hotl_engine::{EngineEvent, Outcome};
use serde_json::{json, Value};

/// Stable schema version of the `-p --json` event stream (an MD Tier-1
/// contract; bump only on a breaking change to a frame's shape).
///
/// **v2** — `turn_done.outcome` changed from a Rust `Debug` string
/// (`"Done { text: \"…\" }"`) to the tagged object `{"kind": …, …}`, and
/// `thinking_delta` gained `text`. Pinned by `tests/json_stream_schema.rs`.
pub const JSON_STREAM_SCHEMA_VERSION: u32 = 2;

/// Streamable events, as clients see them. `None` = not a stream frame
/// (`Ask`/`Question` are round-trips; `TurnDone` is a result) — those callers
/// build their own envelope.
pub fn update_frame(event: &EngineEvent) -> Option<Value> {
    Some(match event {
        EngineEvent::TextDelta(t) => json!({"type": "text_delta", "text": t}),
        EngineEvent::ThinkingDelta(t) => json!({"type": "thinking_delta", "text": t}),
        EngineEvent::ToolStart { name, summary } => {
            json!({"type": "tool_start", "name": name, "summary": summary})
        }
        EngineEvent::ToolDone { name, ok } => {
            json!({"type": "tool_done", "name": name, "ok": ok})
        }
        EngineEvent::ToolDenied { name } => json!({"type": "tool_denied", "name": name}),
        EngineEvent::ToolAutoAllowed { name, rule } => {
            json!({"type": "tool_auto_allowed", "name": name, "rule": rule})
        }
        EngineEvent::Retrying { attempt, reason } => {
            json!({"type": "retrying", "attempt": attempt, "reason": reason})
        }
        EngineEvent::FallbackModel { model } => {
            json!({"type": "fallback_model", "model": model})
        }
        EngineEvent::PromptQueued => json!({"type": "prompt_queued"}),
        EngineEvent::Compacted { degraded } => {
            json!({"type": "compacted", "degraded": degraded})
        }
        EngineEvent::TodosChanged { items } => json!({"type": "todos_changed", "items": items}),
        EngineEvent::Ask { .. } | EngineEvent::Question { .. } | EngineEvent::TurnDone { .. } => {
            return None
        }
    })
}

/// A turn outcome as a tagged object. The headless stream used
/// `format!("{outcome:?}")` instead — unparseable, and un-versionable, since a
/// field rename in `Outcome` would have changed the wire silently.
pub fn outcome_frame(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Done { text } => json!({"kind": "done", "text": text}),
        Outcome::Cancelled => json!({"kind": "cancelled"}),
        Outcome::TurnLimit => json!({"kind": "turn_limit"}),
        Outcome::Refused => json!({"kind": "refused"}),
        Outcome::DoomLoop { pattern } => json!({"kind": "doom_loop", "pattern": pattern}),
        Outcome::ToolFailureBudget { tool } => {
            json!({"kind": "tool_failure_budget", "tool": tool})
        }
        Outcome::Error { message } => json!({"kind": "error", "message": message}),
    }
}

/// One `-p --json` line: an update frame, or the turn-done/ask/question frames
/// only that stream emits — always version-stamped.
///
/// Pure and total: it returns a `Value` for *every* variant, including the two
/// that need side effects (`Ask` and `Question` must resolve their reply
/// channels). Those side effects stay in `agent.rs::render_json`; only the
/// shape lives here, which is what makes the whole stream pin-testable.
pub fn json_frame(event: &EngineEvent) -> Value {
    let mut v = match update_frame(event) {
        Some(v) => v,
        None => match event {
            EngineEvent::TurnDone { outcome, usage } => json!({
                "type": "turn_done",
                "outcome": outcome_frame(outcome),
                "usage": usage,
            }),
            EngineEvent::Ask { summary, .. } => {
                json!({"type": "ask_denied", "summary": summary})
            }
            EngineEvent::Question { question, .. } => json!({
                "type": "question_no_human",
                "header": question.header,
                "prompt": question.prompt,
                "options": question.options,
            }),
            _ => unreachable!("update_frame covers every other variant"),
        },
    };
    v["schema_version"] = json!(JSON_STREAM_SCHEMA_VERSION);
    v
}
