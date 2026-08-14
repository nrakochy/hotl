//! Per-category context accounting for `/context` (plan 0028).
//!
//! INVARIANT: every `Item` contributes to exactly one row, and the rows sum to
//! `tokens::estimate_items` over the same slices plus the non-projection terms.
//! Enforced by `rows_sum_to_the_flat_estimate` and
//! `every_synthetic_reason_lands_in_exactly_one_row`.
//!
//! Rows are emitted for every kind, zeros included: the wire shape stays
//! constant across sessions (so two reports diff line-for-line) and the
//! decision to hide a row lives in one place, client-side.

use crate::tokens::{estimate_item, estimate_text, IMAGE_TOKENS_ESTIMATE};
use hotl_types::{ContextBreakdown, ContextKind, ContextRow, Item, SyntheticReason};

/// The three tool rows, pre-summed by the caller. Counts rather than
/// `&[ToolDef]` because `ToolDef` lives in `hotl-provider`, and depending on
/// that crate from here would invert the layering — the engine holds the
/// `Registry`, so the engine does the walk and the name split.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolTokens {
    pub schemas: u64,
    pub skills: u64,
    pub agents: u64,
}

/// Canonical row order. Mirrors `ContextKind`'s declaration order minus
/// `Unknown`, which the engine never emits — it exists to absorb a *newer*
/// engine's row on an older client.
const ROWS: [ContextKind; 12] = [
    ContextKind::SystemPrompt,
    ContextKind::ToolSchemas,
    ContextKind::SkillsRoster,
    ContextKind::AgentsRoster,
    ContextKind::ProjectInstructions,
    ContextKind::Memory,
    ContextKind::Todos,
    ContextKind::FoldedHistory,
    ContextKind::Messages,
    ContextKind::ToolResults,
    ContextKind::HarnessInjections,
    ContextKind::Images,
];

/// Which row an item's *text* belongs to. Its images are billed separately —
/// see [`image_term`].
pub fn classify(item: &Item) -> ContextKind {
    match item {
        Item::System { .. } | Item::Assistant { .. } => ContextKind::Messages,
        Item::ToolResults { .. } => ContextKind::ToolResults,
        // An item kind this binary does not know: counted, never dropped.
        Item::Unknown => ContextKind::HarnessInjections,
        Item::User { synthetic, .. } => match synthetic {
            None | Some(SyntheticReason::Steer) => ContextKind::Messages,
            Some(SyntheticReason::ProjectInstructions | SyntheticReason::SubdirInstructions) => {
                ContextKind::ProjectInstructions
            }
            Some(SyntheticReason::Memory) => ContextKind::Memory,
            Some(SyntheticReason::Todos) => ContextKind::Todos,
            Some(SyntheticReason::CompactionSummary) => ContextKind::FoldedHistory,
            Some(
                SyntheticReason::SystemReminder
                | SyntheticReason::Moim
                | SyntheticReason::DoomLoopNudge
                | SyntheticReason::RetryFeedback
                | SyntheticReason::SubagentResult
                | SyntheticReason::Environment
                | SyntheticReason::Unknown,
            ) => ContextKind::HarnessInjections,
        },
    }
}

/// The image tokens `estimate_item` already folded into this item's total.
/// Splitting images into their own row means subtracting this from the owning
/// row, or the rows double-count and stop summing to the flat estimate.
pub fn image_term(item: &Item) -> u64 {
    match item {
        Item::User { images, .. } => images.len() as u64 * IMAGE_TOKENS_ESTIMATE,
        _ => 0,
    }
}

/// The whole breakdown, in canonical row order, zeros included.
///
/// `tail` is the ephemeral per-sample suffix (the todo reminder), so it is
/// billed to `Todos` wholesale rather than re-classified.
///
/// `window` is carried through untouched: free space is a display concept the
/// client derives from `window`, the row sum, and the provider's own figure.
pub fn breakdown(
    system: &str,
    tools: ToolTokens,
    durable: &[Item],
    tail: &[Item],
    window: u64,
) -> ContextBreakdown {
    let mut acc = [0u64; ROWS.len()];
    let mut add = |kind: ContextKind, n: u64| {
        let i = ROWS.iter().position(|k| *k == kind).expect("kind is a row");
        acc[i] += n;
    };

    add(ContextKind::SystemPrompt, estimate_text(system));
    add(ContextKind::ToolSchemas, tools.schemas);
    add(ContextKind::SkillsRoster, tools.skills);
    add(ContextKind::AgentsRoster, tools.agents);

    for item in durable {
        let images = image_term(item);
        add(classify(item), estimate_item(item) - images);
        add(ContextKind::Images, images);
    }
    for item in tail {
        let images = image_term(item);
        add(ContextKind::Todos, estimate_item(item) - images);
        add(ContextKind::Images, images);
    }

    ContextBreakdown {
        window,
        rows: ROWS
            .iter()
            .zip(acc)
            .map(|(kind, tokens)| ContextRow {
                kind: *kind,
                tokens,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::estimate_items;
    use hotl_types::{ToolResultItem, UserImage};

    fn user(text: &str, synthetic: Option<SyntheticReason>) -> Item {
        Item::User {
            text: text.into(),
            synthetic,
            images: Vec::new(),
        }
    }

    fn row(b: &ContextBreakdown, kind: ContextKind) -> u64 {
        b.rows
            .iter()
            .find(|r| r.kind == kind)
            .expect("every row is emitted")
            .tokens
    }

    const ALL_REASONS: [SyntheticReason; 13] = [
        SyntheticReason::ProjectInstructions,
        SyntheticReason::SystemReminder,
        SyntheticReason::Steer,
        SyntheticReason::CompactionSummary,
        SyntheticReason::SubagentResult,
        SyntheticReason::DoomLoopNudge,
        SyntheticReason::RetryFeedback,
        SyntheticReason::Moim,
        SyntheticReason::Memory,
        SyntheticReason::SubdirInstructions,
        SyntheticReason::Todos,
        SyntheticReason::Environment,
        SyntheticReason::Unknown,
    ];

    /// Every `Item` variant and every `SyntheticReason`, plus images.
    fn kitchen_sink() -> Vec<Item> {
        let mut items = vec![
            Item::System {
                text: "you are hotl".into(),
            },
            Item::Assistant {
                blocks: vec![serde_json::json!({"type":"text","text":"sure"})],
            },
            Item::ToolResults {
                results: vec![ToolResultItem {
                    tool_use_id: "t1".into(),
                    content: "file contents".repeat(20),
                    is_error: false,
                }],
            },
            Item::Unknown,
            Item::User {
                text: "look at this".into(),
                synthetic: None,
                images: vec![UserImage {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }],
            },
        ];
        for r in ALL_REASONS {
            items.push(user(&format!("{r:?} body text"), Some(r)));
        }
        items
    }

    #[test]
    fn rows_sum_to_the_flat_estimate() {
        let durable = kitchen_sink();
        let tail = vec![user("<todos>…</todos>", Some(SyntheticReason::Todos))];
        let b = breakdown("", ToolTokens::default(), &durable, &tail, 200_000);
        let sum: u64 = b.rows.iter().map(|r| r.tokens).sum();
        assert_eq!(sum, estimate_items(&durable) + estimate_items(&tail));
    }

    #[test]
    fn every_synthetic_reason_lands_in_exactly_one_row() {
        for r in ALL_REASONS {
            let items = vec![user("some meaningful body text here", Some(r))];
            let b = breakdown("", ToolTokens::default(), &items, &[], 200_000);
            let hit: Vec<_> = b.rows.iter().filter(|row| row.tokens > 0).collect();
            assert_eq!(hit.len(), 1, "{r:?} landed in {hit:?}");
            assert_eq!(hit[0].kind, classify(&items[0]));
        }
    }

    #[test]
    fn an_unknown_synthetic_reason_is_counted_not_dropped() {
        let items = vec![user(
            "a tag from the future",
            Some(SyntheticReason::Unknown),
        )];
        let b = breakdown("", ToolTokens::default(), &items, &[], 200_000);
        assert!(row(&b, ContextKind::HarnessInjections) > 0);
    }

    #[test]
    fn images_are_not_double_counted() {
        let img = UserImage {
            media_type: "image/png".into(),
            data: "A".repeat(4096).into(),
        };
        let with = vec![Item::User {
            text: "two pictures".into(),
            synthetic: None,
            images: vec![img.clone(), img],
        }];
        let without = vec![user("two pictures", None)];
        let b = breakdown("", ToolTokens::default(), &with, &[], 200_000);
        assert_eq!(row(&b, ContextKind::Images), 2 * IMAGE_TOKENS_ESTIMATE);
        let plain = breakdown("", ToolTokens::default(), &without, &[], 200_000);
        assert_eq!(
            row(&b, ContextKind::Messages),
            row(&plain, ContextKind::Messages)
        );
    }

    #[test]
    fn the_ephemeral_tail_lands_in_todos() {
        let tail = vec![user(
            "<todo-reminder/>",
            Some(SyntheticReason::SystemReminder),
        )];
        let b = breakdown("", ToolTokens::default(), &[], &tail, 200_000);
        // Even a system-reminder-tagged tail item is billed to todos: the tail
        // IS the reminder, and re-classifying it would scatter one line across
        // two rows.
        assert_eq!(row(&b, ContextKind::Todos), estimate_items(&tail));
        for r in &b.rows {
            assert!(r.tokens == 0 || r.kind == ContextKind::Todos, "{r:?}");
        }
    }

    #[test]
    fn an_empty_session_emits_every_row_at_zero() {
        let b = breakdown("", ToolTokens::default(), &[], &[], 200_000);
        assert_eq!(b.rows.len(), ROWS.len());
        assert!(b.rows.iter().all(|r| r.tokens == 0));
        assert_eq!(b.window, 200_000);
    }

    #[test]
    fn rows_arrive_in_canonical_order() {
        let b = breakdown("sys", ToolTokens::default(), &kitchen_sink(), &[], 0);
        let kinds: Vec<_> = b.rows.iter().map(|r| r.kind).collect();
        let mut sorted = kinds.clone();
        sorted.sort();
        assert_eq!(kinds, sorted);
        assert_eq!(kinds, ROWS.to_vec());
    }

    #[test]
    fn the_tool_rows_are_kept_apart_and_the_system_prompt_is_its_own() {
        let tools = ToolTokens {
            schemas: 100,
            skills: 20,
            agents: 3,
        };
        let b = breakdown("system prompt text", tools, &[], &[], 200_000);
        assert_eq!(row(&b, ContextKind::ToolSchemas), 100);
        assert_eq!(row(&b, ContextKind::SkillsRoster), 20);
        assert_eq!(row(&b, ContextKind::AgentsRoster), 3);
        assert_eq!(
            row(&b, ContextKind::SystemPrompt),
            estimate_text("system prompt text")
        );
    }
}
