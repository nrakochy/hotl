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
/// BOTH `tail_budget` tokens and `image_budget` base64 image bytes (keeping
/// the most verbatim history that fits); if no tail fits, keeps the minimal
/// clean tail and folds everything else. `None` means nothing can fold — the
/// caller must surface context exhaustion rather than loop.
pub fn plan<I: std::borrow::Borrow<Item>>(
    items: &[I],
    tail_budget: u64,
    image_budget: usize,
) -> Option<Plan> {
    let fits = |b: usize| {
        estimate_items(&items[b..]) <= tail_budget
            && crate::tokens::image_b64_bytes(&items[b..]) <= image_budget
    };
    let prefix_end = preserved_prefix_len(items);
    let boundaries: Vec<usize> = (prefix_end + 1..items.len())
        .filter(|&i| {
            matches!(
                items[i].borrow(),
                Item::User { .. } | Item::Assistant { .. }
            )
        })
        .filter(|&i| is_clean_boundary(items, i))
        .collect();
    let latest = *boundaries.last()?;
    let mut chosen = latest;
    for &b in boundaries.iter().rev() {
        if fits(b) {
            chosen = b;
        } else if chosen != latest || b != latest {
            break;
        }
    }
    Some(Plan {
        prefix_end,
        kept_from: chosen,
    })
}

/// The new projection: preserved prefix + digest + pinned results + verbatim
/// tail. `Arc` elements in and out (0033 Task 5): the kept ranges move as
/// pointer clones, only the digest items are newly allocated.
///
/// `pins` are `tool_use` ids a `PreCompact` hook asked to keep (0057 T3).
/// Each pinned result is re-appended as a **standalone** `ToolResults` item
/// right after the digest — never in place, which would strand it behind the
/// assistant turn that called it. Pairing survives because the digest is a
/// user item: the pinned block sits in the folded region's stead, with no
/// `tool_use` in front of it, so it reads as evidence rather than as a reply.
pub fn apply(
    items: &[std::sync::Arc<Item>],
    plan: &Plan,
    digest: &[Item],
    pins: &[String],
) -> Vec<std::sync::Arc<Item>> {
    let pinned = hotl_types::pinned_items(&items[plan.prefix_end..plan.kept_from], pins);
    let mut out = Vec::with_capacity(
        plan.prefix_end + digest.len() + pinned.len() + items.len() - plan.kept_from,
    );
    out.extend_from_slice(&items[..plan.prefix_end]);
    out.extend(digest.iter().cloned().map(std::sync::Arc::new));
    out.extend(pinned.into_iter().map(std::sync::Arc::new));
    out.extend_from_slice(&items[plan.kept_from..]);
    out
}

/// A tail may only start where tool results still sit behind the assistant
/// turn that called them. Starting at a user item that is answered by results
/// would leave those results with no calls in front of them — the request is
/// then rejected for having more tool_result blocks than the preceding turn
/// has tool_use blocks, and the fold makes it permanent.
fn is_clean_boundary<I: std::borrow::Borrow<Item>>(items: &[I], i: usize) -> bool {
    !matches!(
        items.get(i + 1).map(std::borrow::Borrow::borrow),
        Some(Item::ToolResults { .. })
    ) || matches!(items[i].borrow(), Item::Assistant { .. })
}

/// Leading System / ProjectInstructions / Memory items never fold — they are
/// the byte-stable prefix (L6) and the cheapest tokens in the window.
fn preserved_prefix_len<I: std::borrow::Borrow<Item>>(items: &[I]) -> usize {
    items
        .iter()
        .position(|i| {
            !matches!(
                i.borrow(),
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
You compress an agent-session transcript into a working digest that replaces \
it. Two rules decide what happens to each thing you find.\n\
\n\
COPY VERBATIM — reproduce the original words, in quotes where they are the \
user's:\n\
- every correction or constraint the user stated, especially anything spelled \
MUST, MUST NOT, never, always, or only\n\
- every line of the plan's DECISIONS, if a plan is included below\n\
- every validation result that is still open (a failing test, an unfixed \
error, an unanswered question)\n\
- every path that was created, modified or deleted\n\
\n\
SUMMARIZE — keep the finding, drop the bulk:\n\
- tool output and file contents\n\
- approaches that were tried and abandoned, and why\n\
\n\
Output only the digest, structured exactly as:\n\
GOAL: what the user is trying to accomplish\n\
CONSTRAINTS: the user's own words, quoted, one per line\n\
STATE: what has been done and what is true now\n\
DECISIONS: choices made and their reasons\n\
FILES: files touched and how\n\
NEXT: what remains\n\
Be specific (paths, names, values). Omit pleasantries and tool mechanics. \
A section with nothing in it gets the word none, never an invention.";

/// Bytes of a tool result the summarizer reads for the newest
/// [`RECENT_RESULTS`] results — the ones whose detail is still load-bearing.
pub const RESULT_CLIP_RECENT: usize = 2048;
/// Bytes for every older result: enough for the gist, and no more.
pub const RESULT_CLIP: usize = 600;
/// How many trailing results count as recent.
pub const RECENT_RESULTS: usize = 8;

/// Render the folded items as a plain transcript for the summarize call,
/// with the plan artifact (0056) appended when the session has one — its
/// DECISIONS are on the COPY VERBATIM list, and the model cannot copy what
/// it was not shown.
///
/// Tool results are clipped per-item — the digest needs their gist, and the
/// summarize call must stay far smaller than the window being compacted —
/// but the newest [`RECENT_RESULTS`] get [`RESULT_CLIP_RECENT`]: the fold
/// happens *now*, so the work in flight is the work whose detail still
/// matters.
pub fn summarize_prompt<I: std::borrow::Borrow<Item>>(
    folded: &[I],
    plan_md: Option<&str>,
) -> String {
    let mut out = format!(
        "Transcript to compress:\n\n{}",
        render_transcript_clipped(folded, RECENT_RESULTS)
    );
    if let Some(plan) = plan_md.map(str::trim).filter(|p| !p.is_empty()) {
        out.push_str(&format!("\n\nThe session's plan:\n\n{plan}\n"));
    }
    out
}

/// The plain-text transcript both small-model calls read (the compaction
/// summarizer and the goal evaluator): tool results clipped per-item, images
/// text-only — the call must stay far smaller than the window it reads.
///
/// The goal evaluator judges one condition against recent history and never
/// needs the roomier recent clip, so it takes the flat version.
pub(crate) fn render_transcript<I: std::borrow::Borrow<Item>>(items: &[I]) -> String {
    render_transcript_clipped(items, 0)
}

/// `recent` names how many *trailing* tool results get [`RESULT_CLIP_RECENT`]
/// instead of [`RESULT_CLIP`]; `0` clips every result the same.
fn render_transcript_clipped<I: std::borrow::Borrow<Item>>(items: &[I], recent: usize) -> String {
    let total: usize = items
        .iter()
        .map(|i| match i.borrow() {
            Item::ToolResults { results } => results.len(),
            _ => 0,
        })
        .sum();
    let recent_from = total.saturating_sub(recent);
    let mut seen = 0usize;
    let mut out = String::new();
    for item in items {
        match item.borrow() {
            Item::System { .. } | Item::Unknown => {}
            Item::User {
                text, synthetic, ..
            } => {
                let label = if synthetic.is_some() {
                    "user (injected)"
                } else {
                    "user"
                };
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
                    let max = if seen >= recent_from {
                        RESULT_CLIP_RECENT
                    } else {
                        RESULT_CLIP
                    };
                    seen += 1;
                    let clipped = clip(&r.content, max);
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
             the messages that follow it are verbatim; the folded span is \
             retrievable with recall (backend session-log)."
        ),
        synthetic: Some(SyntheticReason::CompactionSummary),
        images: Vec::new(),
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
        images: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::ToolResultItem;
    use serde_json::json;

    fn user(text: &str) -> Item {
        Item::User {
            text: text.into(),
            synthetic: None,
            images: Vec::new(),
        }
    }

    /// A folded image-bearing item digests as text only: the inline
    /// `[Image #N]` marker survives, the base64 never reaches the summarize
    /// call (which must stay far smaller than the window being compacted).
    #[test]
    fn summarize_prompt_renders_image_items_text_only() {
        let items = vec![Item::User {
            text: "look at [Image #1] please".into(),
            synthetic: None,
            images: vec![hotl_types::UserImage {
                media_type: "image/png".into(),
                data: "QkFTRTY0UEFZTE9BRA==".repeat(100).into(),
            }],
        }];
        let prompt = summarize_prompt(&items, None);
        assert!(prompt.contains("[user] look at [Image #1] please"));
        assert!(
            !prompt.contains("QkFTRTY0"),
            "base64 leaked into the digest"
        );
    }
    fn assistant(text: &str) -> Item {
        Item::Assistant {
            blocks: vec![json!({"type":"text","text":text})],
        }
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

    /// History written before steers were held can hold a user item between an
    /// assistant turn and its results. Cutting there would strand the results
    /// permanently, so that boundary is not offered.
    #[test]
    fn tail_never_starts_where_it_would_strand_results() {
        let items = vec![
            user("start"),
            assistant("calling"),
            user("a steer that landed in the gap"),
            results(&"x".repeat(3000)),
            assistant("done"),
        ];
        let plan = plan(&items, 10, usize::MAX).expect("plan");
        assert_ne!(plan.kept_from, 2, "that cut orphans the results");
        let tail = &items[plan.kept_from..];
        assert!(
            !matches!(tail.first(), Some(Item::ToolResults { .. })),
            "and the tail itself never opens on results"
        );
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
        let plan = plan(&items, 10, usize::MAX).expect("plan");
        assert_eq!(plan.kept_from, 3);
        assert!(matches!(items[plan.kept_from], Item::Assistant { .. }));
    }

    #[test]
    fn generous_budget_keeps_more_history() {
        let items = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let plan = plan(&items, 10_000, usize::MAX).expect("plan");
        // Everything after the first foldable position fits: keep from index 1.
        assert_eq!(plan.kept_from, 1);
    }

    #[test]
    fn prefix_is_preserved_and_nothing_to_fold_is_none() {
        let items = vec![
            Item::User {
                text: "<project-instructions>…</project-instructions>".into(),
                synthetic: Some(SyntheticReason::ProjectInstructions),
                images: Vec::new(),
            },
            user("only prompt"),
        ];
        // Only boundary candidates strictly after the prompt exist — none do.
        assert_eq!(plan(&items, 10, usize::MAX), None);

        let with_history = {
            let mut v = items.clone();
            v.push(assistant("did things"));
            v.push(user("more"));
            v.push(assistant("done"));
            v
        };
        let p = plan(&with_history, 10, usize::MAX).expect("plan");
        assert_eq!(p.prefix_end, 1, "instructions stay out of the fold");
        let digest = [digest_item("GOAL: test")];
        let with_history: Vec<std::sync::Arc<Item>> =
            with_history.into_iter().map(std::sync::Arc::new).collect();
        let applied = apply(&with_history, &p, &digest, &[]);
        assert!(matches!(
            *applied[0],
            Item::User {
                synthetic: Some(SyntheticReason::ProjectInstructions),
                ..
            }
        ));
        assert!(matches!(
            *applied[1],
            Item::User {
                synthetic: Some(SyntheticReason::CompactionSummary),
                ..
            }
        ));
    }

    #[test]
    fn the_plan_folds_past_an_over_budget_image_tail_even_when_tokens_fit() {
        // Three user turns, each carrying an image; tokens are trivial, so only
        // the byte budget can move `kept_from`.
        let img = |n: &str| hotl_types::UserImage {
            media_type: "image/png".into(),
            data: n.repeat(4).into(),
        };
        let items = vec![
            Item::System { text: "sys".into() },
            Item::User {
                text: "a".into(),
                synthetic: None,
                images: vec![img("A")],
            },
            Item::User {
                text: "b".into(),
                synthetic: None,
                images: vec![img("B")],
            },
            Item::User {
                text: "c".into(),
                synthetic: None,
                images: vec![img("C")],
            },
        ];
        // A budget that admits one image but not two.
        let p = plan(&items, u64::MAX, 4).expect("a boundary exists");
        assert_eq!(p.kept_from, 3, "the tail must shrink to the last image");
        // With room for everything, the tail stays as wide as the tokens allow.
        let p = plan(&items, u64::MAX, usize::MAX).expect("a boundary exists");
        assert_eq!(p.kept_from, 2);
    }

    /// `is_clean_boundary` only forbids a boundary immediately followed by
    /// `ToolResults`; it says nothing about two `User` items back to back, so
    /// the "minimal" fallback tail can still hold more than one prompt's
    /// images. `plan` must hand that over-budget tail back (`kept_from =
    /// latest`), never `None` — nothing here can shrink it further, so the
    /// caller's retry loop (bounded by `MAX_COMPACT_STREAK`, not this
    /// function) is what has to notice the bytes didn't drop.
    #[test]
    fn a_minimal_tail_can_still_exceed_the_image_budget() {
        let img = |n: &str| hotl_types::UserImage {
            media_type: "image/png".into(),
            data: n.repeat(4).into(),
        };
        let items = vec![
            assistant("calling"),
            Item::User {
                text: "s1".into(),
                synthetic: None,
                images: vec![img("A")],
            },
            Item::User {
                text: "s2".into(),
                synthetic: None,
                images: vec![img("B")],
            },
            results("tool output"),
        ];
        // Budget admits one image, not two — and the only clean boundary
        // (before "s1") carries both.
        let p = plan(&items, u64::MAX, 4).expect("the fallback still returns a plan");
        assert_eq!(p.kept_from, 1);
        assert!(
            crate::tokens::image_b64_bytes(&items[p.kept_from..]) > 4,
            "the returned tail is still over budget"
        );
    }

    #[test]
    fn summarize_prompt_clips_results() {
        // Ten results: the oldest are clipped hard, so the whole prompt stays
        // far under what an unclipped pair of 5KB results would cost.
        let mut folded = vec![user("goal")];
        folded.extend((0..10).map(|_| results(&"z".repeat(5_000))));
        let prompt = summarize_prompt(&folded, None);
        assert!(prompt.len() < 20_000, "{}", prompt.len());
        assert!(prompt.contains("[user] goal"));
    }

    #[test]
    fn summarize_prompt_clips_recent_results_at_2048_and_older_at_600() {
        // Twelve results, each distinct and oversized: the last 8 keep 2048
        // bytes, the first 4 keep 600.
        let mut folded = vec![user("goal")];
        for n in 0..12 {
            folded.push(results(&format!("R{n}:{}", "z".repeat(5_000))));
        }
        let prompt = summarize_prompt(&folded, None);
        let lines: Vec<&str> = prompt
            .lines()
            .filter(|l| l.starts_with("[tool result] "))
            .collect();
        assert_eq!(lines.len(), 12);
        let body = |l: &str| l.trim_start_matches("[tool result] ").len();
        for (n, line) in lines.iter().enumerate() {
            let expected = if n < 4 {
                RESULT_CLIP
            } else {
                RESULT_CLIP_RECENT
            };
            assert_eq!(body(line), expected, "result {n}: {}", &line[..40]);
        }
    }

    #[test]
    fn prompt_carries_the_plan_artifact() {
        let folded = vec![user("goal")];
        let plan = "# Plan\n\nDECISIONS\n- keep the ULID, not a hash\n";
        let prompt = summarize_prompt(&folded, Some(plan));
        assert!(prompt.contains("keep the ULID, not a hash"), "{prompt}");
        assert!(prompt.contains("The session's plan:"), "{prompt}");
        // No plan, and an empty one, both leave the prompt alone.
        assert!(!summarize_prompt(&folded, None).contains("The session's plan"));
        assert!(!summarize_prompt(&folded, Some("  \n")).contains("The session's plan"));
    }
}
