//! L1 — canonical conversation types.
//!
//! Pure data + serde. No tokio, no I/O. Forward-compat serde is policy:
//! `#[serde(other)] Unknown` on persisted enums, `format_version` in the
//! session header, optional fields default + skip-when-none.
//!
//! Assistant content is kept as **verbatim provider blocks** (`serde_json::Value`)
//! rather than re-typed structs: signed thinking blocks must echo back to the
//! provider byte-faithfully or replay breaks (review A11), and unknown future
//! block types survive a round-trip losslessly. Typed *views* are provided for
//! the engine (`assistant_text`, `assistant_tool_uses`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped only on breaking changes to the persisted entry format.
pub const FORMAT_VERSION: u32 = 1;

/// Structural provenance on every injected user item (grok 04):
/// no consumer ever parses message text to learn where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntheticReason {
    ProjectInstructions,
    SystemReminder,
    Steer,
    CompactionSummary,
    SubagentResult,
    DoomLoopNudge,
    RetryFeedback,
    Moim,
    Memory,
    SubdirInstructions,
    Todos,
    #[serde(other)]
    Unknown,
}

/// One conversation item. Internally tagged so `#[serde(other)]` can absorb
/// item kinds this binary doesn't know yet (payload dropped, no crash).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    System {
        text: String,
    },
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        synthetic: Option<SyntheticReason>,
    },
    /// Verbatim provider content blocks (text / tool_use / thinking / ...).
    Assistant {
        blocks: Vec<Value>,
    },
    /// All results for one assistant turn's tool calls, in source order
    /// (the API requires them in a single user message).
    ToolResults {
        results: Vec<ToolResultItem>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultItem {
    pub tool_use_id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

/// A session checklist item (`todo_write`, M4/tier-1 gap #3). Full-state
/// replace: the model rewrites the whole list each call, so there is no
/// separate id/patch shape to reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
    /// Present-tense form shown while in progress ("wiring the gate"); optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

/// A tool invocation extracted from assistant blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Concatenated text of the assistant's text blocks.
pub fn assistant_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

/// Tool-use blocks in source order.
pub fn assistant_tool_uses(blocks: &[Value]) -> Vec<ToolUse> {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| {
            Some(ToolUse {
                id: b.get("id")?.as_str()?.to_string(),
                name: b.get("name")?.as_str()?.to_string(),
                input: b.get("input").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Why a sample stopped. `Other` absorbs stop reasons newer than this binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    PauseTurn,
    Refusal,
    #[serde(other)]
    Other,
}

/// Normalized usage; fields absent from a provider response default to zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.cache_read_input_tokens += rhs.cache_read_input_tokens;
        self.cache_creation_input_tokens += rhs.cache_creation_input_tokens;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub format_version: u32,
    pub session_id: String,
    /// Reserved for fork/resume (M3); always serialized so old logs stay readable.
    pub parent_session_id: Option<String>,
    pub model: String,
    pub created_at_ms: u64,
}

/// One appended log record. `parent_id` forms a chain (a tree from M3);
/// M0 logs are strictly linear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub parent_id: Option<String>,
    pub ts_ms: u64,
    pub payload: EntryPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryPayload {
    Header {
        header: SessionHeader,
    },
    Item {
        item: Item,
    },
    Usage {
        usage: TokenUsage,
    },
    Cancelled {
        reason: String,
    },
    /// Compaction re-points the projection: history before `kept_from` is
    /// replaced by `digest` items; the log itself keeps everything.
    Compaction {
        digest: Vec<Item>,
        /// Leading items of the pre-compaction projection preserved verbatim.
        prefix_end: usize,
        /// Index into the pre-compaction projection where the verbatim tail
        /// starts. Both indices are relative to the projection *at compaction
        /// time*; replay reconstructs by applying compactions in log order.
        kept_from: usize,
        /// True when the summarize call failed and the floor was applied.
        degraded: bool,
    },
    /// Re-point the projection to its first `keep_items` items — the
    /// `branch_move` of the commit-protocol vocabulary, expressed against
    /// the linear projection (M3b). Fork UIs arrive with M4; the entry and
    /// its replay semantics are settled here.
    BranchMove {
        keep_items: usize,
    },
    /// Digest of an abandoned branch, appended after a `branch_move` so the
    /// lesson survives without the tokens (commit-protocol `supersede`).
    Supersede {
        digest: Vec<Item>,
    },
    /// A permission ask committed **before** it surfaces (durable asks):
    /// if the process dies before a matching `ask_resolved`, replay
    /// sees a dangling ask and resume re-surfaces it. Log-only (not a
    /// projection item).
    PendingAsk {
        id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protected_why: Option<String>,
    },
    /// Resolution of a `pending_ask` (§2b): the human answered.
    AskResolved {
        id: String,
        allowed: bool,
    },
    /// Sets/overwrites the session's display name. Log-only — not a
    /// projection item (like `PendingAsk`); the last one wins on replay.
    Rename {
        name: String,
    },
    /// Sets the session's effective permission mode (plan mode's
    /// approve-and-continue, `/mode`, `session/set_mode`). Log-only, like
    /// `Rename` — not a projection item; the last one wins on replay, so
    /// `hotl resume` restores the mode the session was actually in. A
    /// string, not the enum, for forward-compat: the engine maps it.
    ModeSet {
        mode: String,
    },
    /// Durable snapshot of the `todo_write` checklist (M4/tier-1 gap #3).
    /// Log-only, like `Rename`/`ModeSet` — not a projection item, so it never
    /// rides in the model transcript; the last one wins on replay. The live
    /// list itself is ephemeral session context injected as a tagged user
    /// reminder (`SyntheticReason::Todos`), never committed as an `Item`.
    Todos {
        items: Vec<Todo>,
    },
    #[serde(other)]
    Unknown,
}

pub fn new_ulid() -> String {
    ulid::Ulid::new().to_string()
}

/// A session display name: trimmed, non-empty, at most 64 chars.
/// The one validator every entry point (CLI, ACP, TUI) funnels through.
pub fn normalize_session_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (!name.is_empty() && name.chars().count() <= 64).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: Serialize + for<'a> Deserialize<'a>>(v: &T) -> String {
        let a = serde_json::to_string(v).unwrap();
        let back: T = serde_json::from_str(&a).unwrap();
        let b = serde_json::to_string(&back).unwrap();
        assert_eq!(
            a, b,
            "serialize → deserialize → serialize must be byte-identical"
        );
        a
    }

    #[test]
    fn items_roundtrip_byte_identical() {
        let items = vec![
            Item::System {
                text: "you are hotl".into(),
            },
            Item::User {
                text: "hi".into(),
                synthetic: None,
            },
            Item::User {
                text: "<project-instructions>...</project-instructions>".into(),
                synthetic: Some(SyntheticReason::ProjectInstructions),
            },
            Item::Assistant {
                blocks: vec![
                    serde_json::json!({"type":"thinking","thinking":"","signature":"sig=="}),
                    serde_json::json!({"type":"text","text":"hello"}),
                    serde_json::json!({"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a.rs"}}),
                ],
            },
            Item::ToolResults {
                results: vec![ToolResultItem {
                    tool_use_id: "toolu_1".into(),
                    content: "fn main() {}".into(),
                    is_error: false,
                }],
            },
        ];
        for item in &items {
            roundtrip(item);
        }
    }

    #[test]
    fn entry_roundtrip_and_mutation() {
        let mut e = Entry {
            id: new_ulid(),
            parent_id: None,
            ts_ms: 1,
            payload: EntryPayload::Item {
                item: Item::User {
                    text: "x".into(),
                    synthetic: None,
                },
            },
        };
        roundtrip(&e);
        // mutate, re-serialize — still stable
        e.ts_ms = 2;
        roundtrip(&e);
    }

    #[test]
    fn unknown_variants_survive() {
        let item: Item = serde_json::from_str(r#"{"type":"hologram","payload":{"x":1}}"#).unwrap();
        assert_eq!(item, Item::Unknown);
        let reason: SyntheticReason = serde_json::from_str(r#""quantum_nudge""#).unwrap();
        assert_eq!(reason, SyntheticReason::Unknown);
        let payload: EntryPayload =
            serde_json::from_str(r#"{"kind":"visibility","target":"e1"}"#).unwrap();
        assert_eq!(payload, EntryPayload::Unknown);
        let stop: StopReason = serde_json::from_str(r#""cosmic_ray""#).unwrap();
        assert_eq!(stop, StopReason::Other);
    }

    #[test]
    fn assistant_views() {
        let blocks = vec![
            serde_json::json!({"type":"text","text":"I'll read "}),
            serde_json::json!({"type":"text","text":"the file."}),
            serde_json::json!({"type":"tool_use","id":"t1","name":"read","input":{"path":"x"}}),
        ];
        assert_eq!(assistant_text(&blocks), "I'll read the file.");
        let uses = assistant_tool_uses(&blocks);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].name, "read");
    }

    #[test]
    fn rename_entry_roundtrips_with_snake_case_kind() {
        let json = roundtrip(&EntryPayload::Rename {
            name: "fix-auth".into(),
        });
        assert!(json.contains("\"kind\":\"rename\""), "wire kind: {json}");
        assert!(json.contains("\"name\":\"fix-auth\""), "wire name: {json}");
    }

    #[test]
    fn mode_set_entry_roundtrips_snake_case() {
        let j = serde_json::to_string(&EntryPayload::ModeSet {
            mode: "plan".into(),
        })
        .unwrap();
        assert!(j.contains("\"kind\":\"mode_set\""), "wire kind: {j}");
        let back: EntryPayload = serde_json::from_str(&j).unwrap();
        assert_eq!(
            back,
            EntryPayload::ModeSet {
                mode: "plan".into()
            }
        );
    }

    #[test]
    fn todo_types_roundtrip_and_absorb_unknown_status() {
        let t = Todo {
            content: "wire the gate".into(),
            status: TodoStatus::InProgress,
            active_form: Some("wiring the gate".into()),
        };
        let j = serde_json::to_string(&t).unwrap();
        assert!(j.contains("\"status\":\"in_progress\""));
        let back: Todo = serde_json::from_str(&j).unwrap();
        assert_eq!(back, t);
        let unk: TodoStatus = serde_json::from_str("\"blocked_on_ci\"").unwrap();
        assert_eq!(unk, TodoStatus::Unknown);
        let e = EntryPayload::Todos { items: vec![t] };
        let ej = serde_json::to_string(&e).unwrap();
        assert!(ej.contains("\"kind\":\"todos\""));
        assert_eq!(serde_json::from_str::<EntryPayload>(&ej).unwrap(), e);
    }

    #[test]
    fn normalize_session_name_trims_and_bounds() {
        assert_eq!(
            normalize_session_name("  fix auth  "),
            Some("fix auth".into())
        );
        assert_eq!(normalize_session_name("   "), None);
        assert_eq!(normalize_session_name(""), None);
        let long = "x".repeat(65);
        assert_eq!(normalize_session_name(&long), None);
        let max = "é".repeat(64); // chars, not bytes
        assert_eq!(normalize_session_name(&max), Some(max.clone()));
    }
}
