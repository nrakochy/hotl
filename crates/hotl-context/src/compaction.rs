//! Compaction planning + assembly (M2).
//!
//! Pure functions: the engine owns the trigger and the summarize call; this
//! module decides *what* folds and assembles the new projection. The shape is
//! always `preserved prefix + typed digest + verbatim tail`, and the tail
//! snaps to a clean boundary so tool_use/tool_result pairing survives
//! (split-turn handling): a tail may start at a User or an Assistant item,
//! never at ToolResults (results must follow their assistant message).

use hotl_types::{Item, SyntheticReason};

use crate::tokens::estimate_items;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Leading items preserved verbatim (system/instructions/memory).
    pub prefix_end: usize,
    /// Index where the verbatim tail starts; `[prefix_end..kept_from)` folds.
    pub kept_from: usize,
}

/// Choose what to fold. Picks the earliest clean boundary whose tail fits
/// `tail_budget` tokens (keeping the most verbatim history that fits); if no
/// tail fits, keeps the minimal clean tail and folds everything else.
/// `None` means nothing can fold — the caller must surface context exhaustion
/// rather than loop.
pub fn plan(items: &[Item], tail_budget: u64) -> Option<Plan> {
    let prefix_end = preserved_prefix_len(items);
    let boundaries: Vec<usize> = (prefix_end + 1..items.len())
        .filter(|&i| matches!(items[i], Item::User { .. } | Item::Assistant { .. }))
        .collect();
    let latest = *boundaries.last()?;
    let mut chosen = latest;
    for &b in boundaries.iter().rev() {
        if estimate_items(&items[b..]) <= tail_budget {
            chosen = b;
        } else if chosen != latest || b != latest {
            break;
        }
    }
    Some(Plan { prefix_end, kept_from: chosen })
}

/// The new projection: preserved prefix + digest + verbatim tail.
pub fn apply(items: &[Item], plan: &Plan, digest: &[Item]) -> Vec<Item> {
    let mut out = Vec::with_capacity(plan.prefix_end + digest.len() + items.len() - plan.kept_from);
    out.extend_from_slice(&items[..plan.prefix_end]);
    out.extend_from_slice(digest);
    out.extend_from_slice(&items[plan.kept_from..]);
    out
}

/// Leading System / ProjectInstructions / Memory items never fold — they are
/// the byte-stable prefix (L6) and the cheapest tokens in the window.
fn preserved_prefix_len(items: &[Item]) -> usize {
    items
        .iter()
        .position(|i| {
            !matches!(
                i,
                Item::System { .. }
                    | Item::User {
                        synthetic: Some(
                            SyntheticReason::ProjectInstructions | SyntheticReason::Memory
                        ),
                        ..
                    }
            )
        })
        .unwrap_or(items.len())
}

pub const SUMMARIZE_SYSTEM: &str = "\
You compress an agent-session transcript into a working digest. Output only \
the digest, structured exactly as:\n\
GOAL: what the user is trying to accomplish\n\
STATE: what has been done and what is true now\n\
DECISIONS: choices made and their reasons\n\
FILES: files touched and how\n\
NEXT: what remains\n\
Be specific (paths, names, values). Omit pleasantries and tool mechanics.";

/// Render the folded items as a plain transcript for the summarize call.
/// Tool results are clipped per-item — the digest needs their gist, and the
/// summarize call must stay far smaller than the window being compacted.
pub fn summarize_prompt(folded: &[Item]) -> String {
    const RESULT_CLIP: usize = 600;
    let mut out = String::from("Transcript to compress:\n\n");
    for item in folded {
        match item {
            Item::System { .. } | Item::Unknown => {}
            Item::User { text, synthetic } => {
                let label = if synthetic.is_some() { "user (injected)" } else { "user" };
                out.push_str(&format!("[{label}] {text}\n"));
            }
            Item::Assistant { blocks } => {
                let text = hotl_types::assistant_text(blocks);
                if !text.is_empty() {
                    out.push_str(&format!("[assistant] {text}\n"));
                }
                for tu in hotl_types::assistant_tool_uses(blocks) {
                    out.push_str(&format!("[tool call] {}({})\n", tu.name, tu.input));
                }
            }
            Item::ToolResults { results } => {
                for r in results {
                    let clipped = clip(&r.content, RESULT_CLIP);
                    out.push_str(&format!("[tool result] {clipped}\n"));
                }
            }
        }
    }
    out
}

fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The digest as a provenance-tagged user item.
pub fn digest_item(summary: &str) -> Item {
    Item::User {
        text: format!(
            "<compaction-summary>\n{summary}\n</compaction-summary>\n\
             Earlier conversation was compacted into the summary above; \
             the messages that follow it are verbatim."
        ),
        synthetic: Some(SyntheticReason::CompactionSummary),
    }
}

/// The degradation floor: every summarize attempt failed, so the
/// session continues with an honest placeholder instead of bricking.
pub fn floor_digest() -> Item {
    Item::User {
        text: "<compaction-summary degraded=\"true\">\n\
               Earlier conversation was dropped to stay within the context \
               window; a summary could not be generated. Ask the user to \
               restate anything essential from before this point.\n\
               </compaction-summary>"
            .into(),
        synthetic: Some(SyntheticReason::CompactionSummary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::ToolResultItem;
    use serde_json::json;

    fn user(text: &str) -> Item {
        Item::User { text: text.into(), synthetic: None }
    }
    fn assistant(text: &str) -> Item {
        Item::Assistant { blocks: vec![json!({"type":"text","text":text})] }
    }
    fn results(content: &str) -> Item {
        Item::ToolResults {
            results: vec![ToolResultItem {
                tool_use_id: "t".into(),
                content: content.into(),
                is_error: false,
            }],
        }
    }

    #[test]
    fn tail_never_starts_at_tool_results() {
        let items = vec![
            user("start"),
            assistant("calling"),
            results(&"x".repeat(3000)),
            assistant("calling again"),
            results(&"y".repeat(3000)),
        ];
        // Tiny budget: even the minimal tail exceeds it — the plan must still
        // pick a clean boundary (the last assistant), never the results item.
        let plan = plan(&items, 10).expect("plan");
        assert_eq!(plan.kept_from, 3);
        assert!(matches!(items[plan.kept_from], Item::Assistant { .. }));
    }

    #[test]
    fn generous_budget_keeps_more_history() {
        let items = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let plan = plan(&items, 10_000).expect("plan");
        // Everything after the first foldable position fits: keep from index 1.
        assert_eq!(plan.kept_from, 1);
    }

    #[test]
    fn prefix_is_preserved_and_nothing_to_fold_is_none() {
        let items = vec![
            Item::User {
                text: "<project-instructions>…</project-instructions>".into(),
                synthetic: Some(SyntheticReason::ProjectInstructions),
            },
            user("only prompt"),
        ];
        // Only boundary candidates strictly after the prompt exist — none do.
        assert_eq!(plan(&items, 10), None);

        let with_history = {
            let mut v = items.clone();
            v.push(assistant("did things"));
            v.push(user("more"));
            v.push(assistant("done"));
            v
        };
        let p = plan(&with_history, 10).expect("plan");
        assert_eq!(p.prefix_end, 1, "instructions stay out of the fold");
        let digest = [digest_item("GOAL: test")];
        let applied = apply(&with_history, &p, &digest);
        assert!(matches!(
            applied[0],
            Item::User { synthetic: Some(SyntheticReason::ProjectInstructions), .. }
        ));
        assert!(matches!(
            applied[1],
            Item::User { synthetic: Some(SyntheticReason::CompactionSummary), .. }
        ));
    }

    #[test]
    fn summarize_prompt_clips_results() {
        let folded = vec![user("goal"), results(&"z".repeat(5000))];
        let prompt = summarize_prompt(&folded);
        assert!(prompt.len() < 2000);
        assert!(prompt.contains("[user] goal"));
    }
}
