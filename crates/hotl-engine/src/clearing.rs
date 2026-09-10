//! The cheapest rung of the context ladder (0057): pick old tool results to
//! replace with `<cleared/>` stubs, before any digest is paid for.
//!
//! Pure — the engine owns the trigger (`turn.rs`) and the commit
//! (`actor.rs`); this module only decides *which* results are eligible.
//!
//! INVARIANT: a result the model is still working with is never a candidate —
//! the last [`EngineConfig::keep_results_turns`](crate::EngineConfig) user
//! turns, the current turn, and every `skill` result are excluded. Enforced by
//! `the_last_turns_are_never_candidates` and `skill_results_are_never_cleared`.

use std::borrow::Borrow;
use std::collections::HashMap;

use hotl_types::Item;

/// Tools whose results are load-bearing instructions rather than transient
/// output: clearing a skill body deletes the procedure the model is following.
const NEVER_CLEARED: &[&str] = &["skill"];

/// Ids of results eligible for clearing: everything strictly before the
/// newest `keep_turns` real user turns, minus [`NEVER_CLEARED`] tools and
/// anything already a stub. Newest-first order is irrelevant — the whole set
/// is cleared in one entry.
pub fn candidates<I: Borrow<Item>>(items: &[I], keep_turns: usize) -> Vec<String> {
    let turn_starts: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, i)| {
            matches!(
                I::borrow(i),
                Item::User {
                    synthetic: None,
                    ..
                }
            )
        })
        .map(|(n, _)| n)
        .collect();
    // Fewer turns than we keep: nothing is old enough yet.
    let Some(cutoff) = turn_starts
        .len()
        .checked_sub(keep_turns)
        .and_then(|n| turn_starts.get(n).copied())
    else {
        return Vec::new();
    };
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut out = Vec::new();
    for item in &items[..cutoff] {
        match item.borrow() {
            // Read straight off the blocks rather than through
            // `assistant_tool_uses`: that borrows the ids into owned Strings,
            // and only the name is wanted here.
            Item::Assistant { blocks } => {
                for b in blocks {
                    if let (Some(id), Some(name)) = (
                        b.get("id").and_then(serde_json::Value::as_str),
                        b.get("name").and_then(serde_json::Value::as_str),
                    ) {
                        names.insert(id, name);
                    }
                }
            }
            Item::ToolResults { results } => {
                for r in results {
                    if hotl_types::is_cleared_stub(&r.content) || r.content.is_empty() {
                        continue;
                    }
                    let name = names.get(r.tool_use_id.as_str()).copied().unwrap_or("");
                    if NEVER_CLEARED.contains(&name) {
                        continue;
                    }
                    out.push(r.tool_use_id.clone());
                }
            }
            _ => {}
        }
    }
    out
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

    fn call(id: &str, name: &str) -> Item {
        Item::Assistant {
            blocks: vec![json!({"type": "tool_use", "id": id, "name": name, "input": {}})],
        }
    }

    fn result(id: &str, content: &str) -> Item {
        Item::ToolResults {
            results: vec![ToolResultItem {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: false,
            }],
        }
    }

    /// Four user turns, one `bash` result each. Keeping 4 leaves nothing;
    /// keeping 2 leaves the two oldest.
    fn four_turns() -> Vec<Item> {
        let mut v = Vec::new();
        for n in 1..=4 {
            v.push(user(&format!("prompt {n}")));
            v.push(call(&format!("t{n}"), "bash"));
            v.push(result(&format!("t{n}"), &format!("output {n}")));
        }
        v
    }

    #[test]
    fn the_last_turns_are_never_candidates() {
        let items = four_turns();
        assert!(candidates(&items, 4).is_empty(), "all four turns are kept");
        assert_eq!(candidates(&items, 2), vec!["t1", "t2"]);
        // The current turn is the newest one, so it is never in the set.
        assert!(!candidates(&items, 1).contains(&"t4".to_string()));
    }

    #[test]
    fn skill_results_are_never_cleared() {
        let mut items = four_turns();
        items[4] = call("t2", "skill");
        assert_eq!(candidates(&items, 2), vec!["t1"]);
    }

    #[test]
    fn a_stub_is_never_re_cleared() {
        let mut items = four_turns();
        items[2] = result("t1", &hotl_types::cleared_stub("t1", 9, 1));
        assert_eq!(candidates(&items, 2), vec!["t2"]);
    }

    #[test]
    fn a_session_shorter_than_the_keep_window_clears_nothing() {
        let items = vec![user("only"), call("t1", "bash"), result("t1", "out")];
        assert!(candidates(&items, 4).is_empty());
    }
}
