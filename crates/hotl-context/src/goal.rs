//! Goal evaluation (`/goal`, 0034) — the pure half, mirroring `compaction`:
//! the engine owns the evaluator call and the continuation loop; this module
//! decides what the evaluator reads and how its reply parses.

use hotl_types::Item;

use crate::compaction::render_transcript;

/// What the evaluator concluded. `EvalFailed` is engine-side vocabulary, not
/// a parse result — an unparseable reply is `None` and the caller fails open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalVerdict {
    NotYetMet,
    Met,
    Impossible,
}

pub const GOAL_EVAL_SYSTEM: &str = "\
You judge whether an agent session has satisfied a completion condition. \
Read the condition and the conversation, then output exactly two lines:\n\
VERDICT: met|not_yet|impossible\n\
REASON: <one short sentence>\n\
Answer met only when the conversation shows the condition is satisfied now. \
Answer impossible only when no further work could ever satisfy it. Otherwise \
answer not_yet, with the reason naming what is still missing.";

/// The evaluator's user prompt: the condition plus the full clipped
/// projection (the per-item tool-result clip bounds it).
pub fn eval_prompt<I: std::borrow::Borrow<Item>>(condition: &str, items: &[I]) -> String {
    format!(
        "Completion condition:\n{condition}\n\nThe conversation so far:\n\n{}",
        render_transcript(items)
    )
}

/// Parse the two-line contract. Case-tolerant, tolerates a trailing period
/// on the verdict word; `None` = fail open (keep the goal, end the turn).
pub fn parse_verdict(text: &str) -> Option<(GoalVerdict, String)> {
    let mut verdict = None;
    let mut reason = String::new();
    for line in text.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("verdict:") {
            verdict = match rest.trim().trim_end_matches('.') {
                "met" => Some(GoalVerdict::Met),
                "not_yet" => Some(GoalVerdict::NotYetMet),
                "impossible" => Some(GoalVerdict::Impossible),
                _ => return None,
            };
        } else if lower.starts_with("reason:") {
            reason = line["reason:".len()..].trim().to_string();
        }
    }
    verdict.map(|v| (v, reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_contract_and_tolerates_case() {
        assert_eq!(
            parse_verdict("VERDICT: met\nREASON: tests pass."),
            Some((GoalVerdict::Met, "tests pass.".into()))
        );
        assert_eq!(
            parse_verdict("verdict: NOT_YET\nreason: no commit yet"),
            Some((GoalVerdict::NotYetMet, "no commit yet".into()))
        );
        assert_eq!(
            parse_verdict("Verdict: impossible.\nReason: the file was deleted"),
            Some((GoalVerdict::Impossible, "the file was deleted".into()))
        );
        // A missing reason still parses — the verdict is the load-bearing half.
        assert_eq!(
            parse_verdict("VERDICT: met"),
            Some((GoalVerdict::Met, String::new()))
        );
    }

    #[test]
    fn parse_rejects_garbage_and_unknown_verdicts() {
        assert_eq!(parse_verdict(""), None);
        assert_eq!(parse_verdict("the goal seems met to me"), None);
        assert_eq!(parse_verdict("VERDICT: maybe\nREASON: unsure"), None);
        assert_eq!(parse_verdict("REASON: only a reason"), None);
    }

    #[test]
    fn eval_prompt_carries_condition_and_clips_no_base64() {
        let items = vec![Item::User {
            text: "look at [Image #1]".into(),
            synthetic: None,
            images: vec![hotl_types::UserImage {
                media_type: "image/png".into(),
                data: "QkFTRTY0UEFZTE9BRA==".repeat(100).into(),
            }],
        }];
        let prompt = eval_prompt("the diagram is described", &items);
        assert!(prompt.contains("Completion condition:\nthe diagram is described"));
        assert!(prompt.contains("[user] look at [Image #1]"));
        assert!(!prompt.contains("QkFTRTY0"), "base64 leaked into the eval");
    }
}
