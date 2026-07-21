//! Anchored token accounting (M2; review A12b).
//!
//! The provider's reported usage is ground truth; this estimator only covers
//! the *delta* — items appended since the last report. It deliberately
//! overcounts (3 chars/token vs ~4 typical): estimation error then degrades
//! to a slightly early compaction, never a context overflow. The o200k
//! tokenizer swap-in stays on the ledger; with usage anchoring the delta
//! error is bounded by one turn's traffic.

use hotl_types::Item;

const CHARS_PER_TOKEN: u64 = 3;
/// Wire framing per item (role, block structure).
const ITEM_OVERHEAD: u64 = 8;

pub fn estimate_text(text: &str) -> u64 {
    (text.len() as u64).div_ceil(CHARS_PER_TOKEN)
}

pub fn estimate_item(item: &Item) -> u64 {
    let body = match item {
        Item::System { text } | Item::User { text, .. } => estimate_text(text),
        // Serialized length covers thinking/signature payloads too — they all
        // ride back up on the next request.
        Item::Assistant { blocks } => blocks.iter().map(|b| estimate_text(&b.to_string())).sum(),
        Item::ToolResults { results } => {
            results.iter().map(|r| estimate_text(&r.content) + 6).sum()
        }
        Item::Unknown => 0,
    };
    ITEM_OVERHEAD + body
}

pub fn estimate_items(items: &[Item]) -> u64 {
    items.iter().map(estimate_item).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_overcount_not_undercount() {
        // ~4 chars/token English text must estimate high, never low.
        let text = "the quick brown fox jumps over the lazy dog. ".repeat(100);
        let actual_ish = text.len() as u64 / 4;
        assert!(estimate_text(&text) > actual_ish);

        let items = vec![
            Item::User {
                text: text.clone(),
                synthetic: None,
            },
            Item::Assistant {
                blocks: vec![serde_json::json!({"type":"text","text":text})],
            },
        ];
        assert!(estimate_items(&items) > 2 * actual_ish);
    }
}
