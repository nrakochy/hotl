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
use std::sync::Arc;

pub mod sanitize;

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
    /// Session-start environment facts (`<env …/>`, 0030 Task 6).
    Environment,
    /// Goal-loop continuation: the evaluator's "not yet met" reason,
    /// injected as the next turn's opening user item (0034).
    GoalGuidance,
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
        /// Attached images. Empty for every synthetic item and for old logs
        /// (`skip_serializing_if` keeps imageless entries byte-identical to
        /// the pre-image shape; old binaries ignore the unknown key and keep
        /// the text, inline `[Image #N]` markers included).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<UserImage>,
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

/// One user-attached image, stored inline (base64) in the log entry.
///
/// Inline — not a blob-sidecar path — is load-bearing: the speculation
/// protocol proves wire bodies byte-identical across two build paths, and a
/// serializer that read files at build time could not make that promise;
/// retention also prunes `.blobs`, which would leave replayed sessions with
/// dangling image references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserImage {
    /// IANA media type: image/png | image/jpeg | image/gif | image/webp.
    pub media_type: String,
    /// Base64 (standard alphabet, padded), no data-URL prefix. `Arc` because
    /// the projection is copy-on-write: a `String` here made every appended
    /// entry memcpy every live image.
    pub data: Arc<str>,
}

/// What fills the context window, by source (`/context`, plan 0028).
///
/// Declaration order IS display order — `Ord` is derived, and both the engine
/// and the TUI sort by it, so reordering these variants reorders the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    SystemPrompt,
    ToolSchemas,
    SkillsRoster,
    AgentsRoster,
    ProjectInstructions,
    Memory,
    Todos,
    FoldedHistory,
    Messages,
    ToolResults,
    HarnessInjections,
    Images,
    /// A row this binary does not know. Absorbs a future engine's new kind
    /// without dropping its tokens on the floor — dropping them would
    /// undercount, the one direction this codebase treats as unacceptable.
    #[serde(other)]
    Unknown,
}

/// One row of a `/context` breakdown. `kind` is a stable wire tag, never a
/// display string — the client owns the label.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextRow {
    pub kind: ContextKind,
    pub tokens: u64,
}

/// The whole `/context` payload. `window` rides along so the client never has
/// to reconcile its handshake value against the engine's live config.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextBreakdown {
    pub window: u64,
    pub rows: Vec<ContextRow>,
}

/// Per-prompt ceiling on total DECODED image bytes.
pub const MAX_PROMPT_DECODED_BYTES: usize = 16 * 1024 * 1024;

/// Ceiling on base64 image bytes alive in one request. Images are the only
/// payload the token estimator deliberately under-charges (a flat 1600 each),
/// so they get their own budget; crossing it forces the fold the token
/// estimate alone would never trigger. 24MB of base64 (≈18MB decoded) leaves
/// ~8MB for system, history text and tool schemas under the ~32MB request cap
/// `MAX_PROMPT_DECODED_BYTES` already reasons about.
///
/// INVARIANT: one prompt's images always fit this budget. Enforced by
/// `one_maximal_prompt_always_fits_the_window_image_budget`. That does not
/// bound a fold's retries by itself — a minimal clean tail can hold more
/// than one prompt (`compaction::is_clean_boundary` allows it) — so the real
/// backstop against looping is `MAX_COMPACT_STREAK` in `hotl_engine::actor`.
pub const IMAGE_B64_BUDGET: usize = 24 * 1024 * 1024;

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

/// `serde(skip_serializing_if)` needs a named function, not an inline
/// closure — this is the zero-check every new `TokenUsage` bucket shares.
fn is_zero_u64(n: &u64) -> bool {
    *n == 0
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
    /// Cache-write tokens billed at the 5-minute TTL rate — a refinement of
    /// `cache_creation_input_tokens` (which stays the authoritative total),
    /// not a replacement for it. `skip_serializing_if` keeps a payload with
    /// no per-TTL breakdown byte-identical to one from before this field
    /// existed: this struct is persisted in the session log and serialized
    /// into `--json`/ACP frames, so a zero bucket must stay invisible on
    /// the wire.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_creation_5m_input_tokens: u64,
    /// Cache-write tokens billed at the 1-hour TTL rate (2x input, vs. 1.25x
    /// for the 5-minute default). See `cache_creation_5m_input_tokens`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_creation_1h_input_tokens: u64,
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.cache_read_input_tokens += rhs.cache_read_input_tokens;
        self.cache_creation_input_tokens += rhs.cache_creation_input_tokens;
        self.cache_creation_5m_input_tokens += rhs.cache_creation_5m_input_tokens;
        self.cache_creation_1h_input_tokens += rhs.cache_creation_1h_input_tokens;
    }
}

impl TokenUsage {
    /// Fraction of prompt tokens (input + cache reads + cache writes) served
    /// from the cache. `None` when there was no cache activity at all (no
    /// reads, no writes) — that is "nothing to report", not a 0% hit rate,
    /// so a plain uncached request never shows a misleading `0%`. Division
    /// by zero never happens: the guard only lets the divide run once the
    /// denominator has a cache-derived term in it.
    pub fn hit_ratio(&self) -> Option<f64> {
        if self.cache_read_input_tokens == 0 && self.cache_creation_input_tokens == 0 {
            return None;
        }
        let total =
            self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens;
        Some(self.cache_read_input_tokens as f64 / total as f64)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub format_version: u32,
    pub session_id: String,
    /// Reserved for fork/resume (M3); always serialized so old logs stay readable.
    pub parent_session_id: Option<String>,
    /// Fork-point pin: the id of the last parent entry that was part of this
    /// session's seed. Ancestor replay stops after it, so a parent whose own
    /// session keeps working post-fork — appending turns, or compacting, which
    /// rewrites the projection *prefix* — cannot retroactively rewrite this
    /// session's inherited history. Only a log's own live session appends to
    /// it, so pinning the fork-time tip fully restores snapshot semantics.
    ///
    /// `None` (every log written before the field existed) = uncapped ancestor
    /// replay, i.e. the original behavior.
    ///
    /// INVARIANT: a pinned child's replay is unaffected by anything the parent
    /// logs after the fork. Enforced by
    /// `a_pinned_child_ignores_parent_entries_appended_after_the_fork` and
    /// `a_pinned_child_survives_a_post_fork_parent_compaction` (hotl-store).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tip_entry_id: Option<String>,
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
    /// Sets the session's effective permission mode (`/mode`,
    /// `session/set_mode`). Log-only, like `Rename` — not a projection item;
    /// the last one wins on replay, so `hotl resume` restores the mode the
    /// session was actually in. A string, not the enum, for forward-compat:
    /// the engine maps it.
    ///
    /// Two legacy values the engine still maps on replay: `"auto"` (renamed
    /// `"bypass"`) and `"plan"` (from when plan was a mode rather than the
    /// separate [`Self::PlanSet`] axis).
    ModeSet {
        mode: String,
    },
    /// Toggles plan mode, the permission axis orthogonal to `ModeSet`
    /// (`/plan`, `session/set_plan`). Log-only, last one wins, exactly like
    /// `ModeSet`.
    PlanSet {
        on: bool,
    },
    /// Sets the session's reasoning depth (`/effort`, `session/set_effort`).
    /// Log-only, last one wins, exactly like `ModeSet`. `None` means "the
    /// provider's own default" and must round-trip: clearing the setting is a
    /// distinct act from never having set one.
    EffortSet {
        effort: Option<String>,
    },
    /// A structured question (`ask_user`, tier-1 gap #4) committed durably
    /// **before** it surfaces — mirrors `PendingAsk`/`AskResolved` exactly:
    /// if the process dies before a matching `question_resolved`, replay
    /// sees a dangling question and resume can re-surface it. Log-only (not
    /// a projection item).
    PendingQuestion {
        id: String,
        question: Question,
    },
    /// Resolution of a `pending_question`: the human's answer (already
    /// formatted to the plain text the model reads — labels joined for a
    /// selection, the free-text body, or the no-human guidance).
    QuestionResolved {
        id: String,
        answer: String,
    },
    /// Durable snapshot of the `todo_write` checklist (M4/tier-1 gap #3).
    /// Log-only, like `Rename`/`ModeSet` — not a projection item, so it never
    /// rides in the model transcript; the last one wins on replay. The live
    /// list itself is ephemeral session context injected as a tagged user
    /// reminder (`SyntheticReason::Todos`), never committed as an `Item`.
    Todos {
        items: Vec<Todo>,
    },
    /// Sets or resolves the session's goal (`/goal`, 0034). Log-only, last
    /// one wins, like `ModeSet`. `condition: None` is the tombstone: an
    /// achieved/cleared goal must never be restored by resume. `outcome`
    /// (`"achieved" | "impossible" | "cleared"`) records why it ended;
    /// absent on set.
    GoalSet {
        condition: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

/// One selectable choice in a structured [`Question`] (`ask_user`, tier-1
/// gap #4). `description` is an optional one-line elaboration shown under
/// the label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A structured multiple-choice question the agent asks the human
/// (`ask_user`) — a header, a prompt, and 2-4 labelled options (plus an
/// always-available free-text "other" the surfaces provide, not encoded
/// here). `multi` reserves multi-select for a future surface; today's
/// surfaces treat every question as single-select.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub header: String,
    pub prompt: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi: bool,
}

/// A human's answer to an `ask_user` question (tier-1 gap #4). Lives here
/// (not hotl-engine, where the plan first sketched it) so both hotl-tools's
/// `QuestionSink` and hotl-engine's `EngineEvent::Question` can share one
/// definition without either crate depending on the other — the same
/// cycle-avoidance the plan flagged as open, resolved the way `Question`
/// itself already is. `NoHuman` is the documented default when no reply
/// arrives (headless, `DontAsk`, a dropped reply channel): the model must
/// always get an answer, never a hang.
#[derive(Debug, Clone, PartialEq)]
pub enum QuestionAnswer {
    Selected(Vec<String>),
    FreeText(String),
    NoHuman,
}

pub fn new_ulid() -> String {
    ulid::Ulid::new().to_string()
}

/// A model id with its `provider/` prefix dropped, for display only.
///
/// Config spells models `provider/model` (`anthropic/claude-opus-5`), and the
/// prefix is dead width on a one-line strip. Only `/` is cut — Bedrock's
/// `anthropic.claude-…` keeps its dot, because no display rule is worth a
/// helper that could bite a version number off some future `o3.5`.
pub fn bare_model(model: &str) -> &str {
    model.split_once('/').map_or(model, |(_, rest)| rest)
}

/// A session display name: trimmed, non-empty, at most 64 chars.
/// The one validator every entry point (CLI, ACP, TUI) funnels through.
pub fn normalize_session_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (!name.is_empty() && name.chars().count() <= 64).then(|| name.to_string())
}

/// A `/goal` condition may run to a paragraph, hence the roomier bound.
pub const GOAL_MAX_CHARS: usize = 4000;

/// A goal condition: trimmed, non-empty, at most [`GOAL_MAX_CHARS`] chars.
/// The one validator every entry point (CLI, ACP, TUI) funnels through.
pub fn normalize_goal(raw: &str) -> Option<String> {
    let goal = raw.trim();
    (!goal.is_empty() && goal.chars().count() <= GOAL_MAX_CHARS).then(|| goal.to_string())
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
                images: Vec::new(),
            },
            Item::User {
                text: "<project-instructions>...</project-instructions>".into(),
                synthetic: Some(SyntheticReason::ProjectInstructions),
                images: Vec::new(),
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

    /// The wire/log shape of an imageless user item is pinned to the exact
    /// pre-`images` bytes: this struct is persisted in session logs and
    /// serialized into provider requests, so an empty vec must stay invisible.
    #[test]
    fn imageless_user_item_serializes_byte_identical_to_before() {
        let plain = Item::User {
            text: "hi".into(),
            synthetic: None,
            images: Vec::new(),
        };
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"type":"user","text":"hi"}"#
        );
        let tagged = Item::User {
            text: "x".into(),
            synthetic: Some(SyntheticReason::Steer),
            images: Vec::new(),
        };
        assert_eq!(
            serde_json::to_string(&tagged).unwrap(),
            r#"{"type":"user","text":"x","synthetic":"steer"}"#
        );
        // Old bytes (no `images` key) deserialize to an empty vec.
        let old: Item = serde_json::from_str(r#"{"type":"user","text":"hi"}"#).unwrap();
        assert_eq!(old, plain);
    }

    #[test]
    fn user_item_with_images_round_trips() {
        let item = Item::User {
            text: "look: [Image #1]".into(),
            synthetic: None,
            images: vec![UserImage {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            }],
        };
        let json = roundtrip(&item);
        assert!(json.contains(r#""images":[{"media_type":"image/png""#));
        let back: Item = serde_json::from_str(&json).unwrap();
        assert_eq!(back, item);
    }

    #[test]
    fn cloning_an_item_shares_the_image_payload_instead_of_copying_it() {
        let item = Item::User {
            text: "look".into(),
            synthetic: None,
            images: vec![UserImage {
                media_type: "image/png".into(),
                data: "aW1nMQ==".into(),
            }],
        };
        let copy = item.clone();
        let (Item::User { images: a, .. }, Item::User { images: b, .. }) = (&item, &copy) else {
            panic!("both are user items");
        };
        // INVARIANT: a projection clone never copies base64. Enforced by this test.
        assert!(std::sync::Arc::ptr_eq(&a[0].data, &b[0].data));
    }

    #[test]
    fn an_image_item_round_trips_and_an_imageless_one_stays_byte_identical() {
        let with = Item::User {
            text: "look".into(),
            synthetic: None,
            images: vec![UserImage {
                media_type: "image/png".into(),
                data: "aW1nMQ==".into(),
            }],
        };
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains(r#""data":"aW1nMQ==""#), "{json}");
        assert_eq!(serde_json::from_str::<Item>(&json).unwrap(), with);

        let without = Item::User {
            text: "look".into(),
            synthetic: None,
            images: Vec::new(),
        };
        let json = serde_json::to_string(&without).unwrap();
        assert!(!json.contains("images"), "{json}");
    }

    #[test]
    fn question_types_and_entries_roundtrip() {
        let q = Question {
            header: "Auth".into(),
            prompt: "Which provider?".into(),
            options: vec![
                QuestionOption {
                    label: "Keycloak".into(),
                    description: Some("self-hosted".into()),
                },
                QuestionOption {
                    label: "Auth0".into(),
                    description: None,
                },
            ],
            multi: false,
        };
        let pj = serde_json::to_string(&EntryPayload::PendingQuestion {
            id: "q1".into(),
            question: q.clone(),
        })
        .unwrap();
        assert!(pj.contains("\"kind\":\"pending_question\""));
        assert_eq!(
            serde_json::from_str::<EntryPayload>(&pj).unwrap(),
            EntryPayload::PendingQuestion {
                id: "q1".into(),
                question: q
            }
        );
        let rj = serde_json::to_string(&EntryPayload::QuestionResolved {
            id: "q1".into(),
            answer: "Keycloak".into(),
        })
        .unwrap();
        assert!(rj.contains("\"kind\":\"question_resolved\""));
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
                    images: Vec::new(),
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
    fn effort_set_entry_roundtrips_including_the_unset_case() {
        let j = serde_json::to_string(&EntryPayload::EffortSet {
            effort: Some("xhigh".into()),
        })
        .unwrap();
        assert!(j.contains("\"kind\":\"effort_set\""), "wire kind: {j}");
        let back: EntryPayload = serde_json::from_str(&j).unwrap();
        assert_eq!(
            back,
            EntryPayload::EffortSet {
                effort: Some("xhigh".into())
            }
        );
        // "cleared" must survive the round trip as its own value.
        let cleared = serde_json::to_string(&EntryPayload::EffortSet { effort: None }).unwrap();
        let back: EntryPayload = serde_json::from_str(&cleared).unwrap();
        assert_eq!(back, EntryPayload::EffortSet { effort: None });
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
    fn hit_ratio_is_absent_without_cache_activity() {
        // Plain uncached usage: no reads, no writes. `Some(0.0)` would read
        // as "0% cache hit"; the correct signal is "no cache info at all".
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            ..Default::default()
        };
        assert_eq!(usage.hit_ratio(), None);
    }

    #[test]
    fn hit_ratio_divides_reads_by_total_prompt_tokens() {
        let usage = TokenUsage {
            input_tokens: 25,
            output_tokens: 10,
            cache_read_input_tokens: 50,
            cache_creation_input_tokens: 25,
            ..Default::default()
        };
        assert_eq!(usage.hit_ratio(), Some(0.5));
    }

    #[test]
    fn hit_ratio_is_present_and_zero_on_a_cache_write_with_no_reads() {
        // A cold prefix write with nothing yet read back: cache activity
        // happened (so the ratio is meaningful), but the hit rate is 0%.
        let usage = TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 100,
            ..Default::default()
        };
        assert_eq!(usage.hit_ratio(), Some(0.0));
    }

    #[test]
    fn hit_ratio_never_divides_by_zero() {
        assert_eq!(TokenUsage::default().hit_ratio(), None);
    }

    #[test]
    fn token_usage_with_zero_ttl_buckets_serializes_byte_identical_to_before() {
        // The pre-change struct's exact JSON shape for this usage value —
        // pinned literally so a future edit that starts emitting the new
        // bucket keys at zero is caught here, not downstream in a session
        // log or a --json consumer.
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: 5,
            cache_creation_input_tokens: 7,
            ..Default::default()
        };
        let json = serde_json::to_string(&usage).unwrap();
        assert_eq!(
            json,
            "{\"input_tokens\":10,\"output_tokens\":20,\"cache_read_input_tokens\":5,\
             \"cache_creation_input_tokens\":7}"
        );
        assert!(!json.contains("cache_creation_5m_input_tokens"));
        assert!(!json.contains("cache_creation_1h_input_tokens"));
        let back: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, usage);
    }

    #[test]
    fn token_usage_with_nonzero_ttl_buckets_round_trips() {
        let usage = TokenUsage {
            input_tokens: 10,
            cache_creation_input_tokens: 300,
            cache_creation_5m_input_tokens: 100,
            cache_creation_1h_input_tokens: 200,
            ..Default::default()
        };
        let json = serde_json::to_string(&usage).unwrap();
        assert!(json.contains("\"cache_creation_5m_input_tokens\":100"));
        assert!(json.contains("\"cache_creation_1h_input_tokens\":200"));
        let back: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, usage);
    }

    #[test]
    fn token_usage_deserializes_old_bytes_with_no_ttl_buckets() {
        // A pre-existing session-log entry / --json frame with none of the
        // new keys must still parse, with both buckets defaulting to zero.
        let old = r#"{"input_tokens":1,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}"#;
        let usage: TokenUsage = serde_json::from_str(old).unwrap();
        assert_eq!(usage.cache_creation_5m_input_tokens, 0);
        assert_eq!(usage.cache_creation_1h_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 4);
    }

    #[test]
    fn one_maximal_prompt_always_fits_the_window_image_budget() {
        // See `IMAGE_B64_BUDGET`'s doc comment for the invariant this proves
        // (and the one it doesn't).
        let worst_case_b64 = MAX_PROMPT_DECODED_BYTES.div_ceil(3) * 4;
        assert!(
            worst_case_b64 <= IMAGE_B64_BUDGET,
            "{worst_case_b64} > {IMAGE_B64_BUDGET}"
        );
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

    #[test]
    fn normalize_goal_trims_and_bounds() {
        assert_eq!(
            normalize_goal("  all tests pass  "),
            Some("all tests pass".into())
        );
        assert_eq!(normalize_goal("   "), None);
        assert_eq!(normalize_goal(""), None);
        let long = "x".repeat(GOAL_MAX_CHARS + 1);
        assert_eq!(normalize_goal(&long), None);
        let max = "é".repeat(GOAL_MAX_CHARS); // chars, not bytes
        assert_eq!(normalize_goal(&max), Some(max.clone()));
    }

    #[test]
    fn goal_set_entry_roundtrips_set_and_tombstone() {
        let set = serde_json::to_string(&EntryPayload::GoalSet {
            condition: Some("all tests pass".into()),
            outcome: None,
        })
        .unwrap();
        assert!(set.contains("\"kind\":\"goal_set\""), "wire kind: {set}");
        assert!(
            !set.contains("outcome"),
            "absent outcome stays absent: {set}"
        );
        let back: EntryPayload = serde_json::from_str(&set).unwrap();
        assert_eq!(
            back,
            EntryPayload::GoalSet {
                condition: Some("all tests pass".into()),
                outcome: None
            }
        );
        // The tombstone: condition cleared, outcome recorded.
        let tomb = serde_json::to_string(&EntryPayload::GoalSet {
            condition: None,
            outcome: Some("achieved".into()),
        })
        .unwrap();
        let back: EntryPayload = serde_json::from_str(&tomb).unwrap();
        assert_eq!(
            back,
            EntryPayload::GoalSet {
                condition: None,
                outcome: Some("achieved".into())
            }
        );
    }

    #[test]
    fn bare_model_drops_only_the_provider_prefix() {
        assert_eq!(bare_model("anthropic/claude-opus-5"), "claude-opus-5");
        assert_eq!(bare_model("openai/gpt-5"), "gpt-5");
        assert_eq!(bare_model("claude-opus-5"), "claude-opus-5");
        assert_eq!(bare_model(""), "");
        // Only the first segment goes: an OpenAI-compatible endpoint whose
        // model name itself contains a slash keeps the rest intact.
        assert_eq!(bare_model("openai/org/model-1"), "org/model-1");
        // Bedrock's dotted spelling is left whole — see the doc comment.
        assert_eq!(
            bare_model("anthropic.claude-opus-5"),
            "anthropic.claude-opus-5"
        );
    }
}
