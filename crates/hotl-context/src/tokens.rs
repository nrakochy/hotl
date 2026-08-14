//! Anchored token accounting (M2; review A12b).
//!
//! The provider's reported usage is ground truth; this estimator only covers
//! the *delta* — items appended since the last report. It deliberately
//! overcounts, so estimation error degrades to a slightly early compaction and
//! never to a context overflow.
//!
//! **Why there is no real tokenizer here** (decision recorded 2026-07-25, see
//! `specs/exec-plans/active/0014-remediation-model-registry.md` Task 4):
//! `o200k` is OpenAI's BPE and Anthropic's is unpublished, so a BPE crate would
//! be a differently-wrong approximation for the default provider — at the cost
//! of a multi-megabyte dependency on a crate that otherwise depends on
//! `hotl-types` alone and is on the reserved WASM path. The exact answer is the
//! provider's own `count_tokens` endpoint; revisit when that is wired.
//!
//! INVARIANT: for any input, the estimate is >= the true token count for
//! ordinary text in every script class. Enforced by
//! `estimates_overcount_not_undercount`,
//! `cjk_is_counted_by_character_not_by_byte_and_never_undercounts`, and
//! `emoji_weigh_more_than_one_token_each`.

use hotl_types::Item;

/// Wire framing per item (role, block structure).
pub const ITEM_OVERHEAD: u64 = 8;
/// Wire framing per `tool_result` block (id, type, is_error).
pub const RESULT_OVERHEAD: u64 = 6;
/// Wire framing per assistant content block (type tag, delimiters).
pub const BLOCK_OVERHEAD: u64 = 4;

/// How a model's tokenizer trades characters for tokens, by script class.
///
/// Three classes rather than one ratio because the classes behave nothing
/// alike: a byte-based estimate happens to land near the truth for CJK and
/// wildly over for ASCII, while a naive chars/3 undercounts CJK by ~3x. The
/// only unacceptable direction is undercounting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenProfile {
    /// ASCII (`< U+0080`) characters per token.
    pub ascii_chars_per_token: f32,
    /// Tokens per non-ASCII BMP character (CJK, Cyrillic, Greek, accented
    /// Latin) — roughly 1 token each.
    pub bmp_weight: f32,
    /// Tokens per astral character (`>= U+10000`: emoji, rare CJK extensions)
    /// — typically 2-4 each.
    pub astral_weight: f32,
}

impl TokenProfile {
    /// The default. `3.0` ASCII chars/token is below the ~4 typical, which is
    /// what preserves the overcount bias the whole design leans on.
    pub const CONSERVATIVE: TokenProfile = TokenProfile {
        ascii_chars_per_token: 3.0,
        bmp_weight: 1.0,
        astral_weight: 3.0,
    };

    /// Build a profile from a catalog row's `ascii_chars_per_token`
    /// (`hotl_provider::catalog::ModelInfo`). Clamped to `>= 1.0`: a bad or
    /// zero ratio must degrade to "one token per character", never to a
    /// division by zero or a zero estimate.
    pub fn from_chars_per_token(ratio: f32) -> Self {
        TokenProfile {
            ascii_chars_per_token: if ratio.is_finite() && ratio >= 1.0 {
                ratio
            } else {
                1.0
            },
            ..Self::CONSERVATIVE
        }
    }
}

impl Default for TokenProfile {
    fn default() -> Self {
        Self::CONSERVATIVE
    }
}

pub fn estimate_text(text: &str) -> u64 {
    estimate_text_with(text, &TokenProfile::CONSERVATIVE)
}

/// Script-aware character count. Counts **characters**, by class — the M2
/// version counted `text.len()`, i.e. bytes, under a constant named
/// `CHARS_PER_TOKEN`.
pub fn estimate_text_with(text: &str, profile: &TokenProfile) -> u64 {
    let (mut ascii, mut bmp, mut astral) = (0u64, 0u64, 0u64);
    for c in text.chars() {
        match c as u32 {
            0..=0x7F => ascii += 1,
            0x80..=0xFFFF => bmp += 1,
            _ => astral += 1,
        }
    }
    let total = (ascii as f64 / profile.ascii_chars_per_token.max(1.0) as f64)
        + (bmp as f64 * profile.bmp_weight as f64)
        + (astral as f64 * profile.astral_weight as f64);
    total.ceil() as u64
}

pub fn estimate_item(item: &Item) -> u64 {
    estimate_item_with(item, &TokenProfile::CONSERVATIVE)
}

/// Flat per-image estimate. Anthropic bills ≈ (w×h)/750 tokens and
/// auto-resizes anything past ~1.15 MP, capping a single image near ~1590;
/// OpenAI's high-detail tiling for typical sizes lands lower. 1600 covers the
/// worst case on either dialect — the estimator's one invariant is
/// never-undercount. The base64 payload must NEVER go through text
/// estimation: a 5MB image would fake ~1.7M tokens and instantly trip the
/// compaction threshold.
pub const IMAGE_TOKENS_ESTIMATE: u64 = 1600;

/// Base64 image bytes in a projection. The estimator charges images a flat
/// token price on purpose, so bytes need their own accounting — this is it.
pub fn image_b64_bytes<I: std::borrow::Borrow<Item>>(items: &[I]) -> usize {
    items
        .iter()
        .map(|i| match i.borrow() {
            Item::User { images, .. } => images.iter().map(|im| im.data.len()).sum(),
            _ => 0,
        })
        .sum()
}

pub fn estimate_item_with(item: &Item, profile: &TokenProfile) -> u64 {
    let body = match item {
        Item::System { text } => estimate_text_with(text, profile),
        Item::User { text, images, .. } => {
            estimate_text_with(text, profile) + images.len() as u64 * IMAGE_TOKENS_ESTIMATE
        }
        Item::Assistant { blocks } => blocks
            .iter()
            .map(|b| BLOCK_OVERHEAD + estimate_block(b, profile))
            .sum(),
        Item::ToolResults { results } => results
            .iter()
            .map(|r| RESULT_OVERHEAD + estimate_text_with(&r.content, profile))
            .sum(),
        Item::Unknown => 0,
    };
    ITEM_OVERHEAD + body
}

/// An assistant block's *content*, not its JSON envelope. The M2 version
/// estimated `block.to_string()`, so `{"type":"text","text":` and one
/// backslash per escaped character were billed as model output. Thinking and
/// signature payloads still count — they ride back up on the next request.
fn estimate_block(block: &serde_json::Value, profile: &TokenProfile) -> u64 {
    let Some(obj) = block.as_object() else {
        return estimate_text_with(&block.to_string(), profile);
    };
    obj.iter()
        .filter(|(k, _)| k.as_str() != "type")
        .map(|(_, v)| match v.as_str() {
            // A string field: its characters are the payload.
            Some(s) => estimate_text_with(s, profile),
            // Anything else (a tool_use `input` object, a number): the
            // serialized form *is* what goes on the wire.
            None => estimate_text_with(&v.to_string(), profile),
        })
        .sum()
}

pub fn estimate_items<I: std::borrow::Borrow<Item>>(items: &[I]) -> u64 {
    estimate_items_with(items, &TokenProfile::CONSERVATIVE)
}

pub fn estimate_items_with<I: std::borrow::Borrow<Item>>(
    items: &[I],
    profile: &TokenProfile,
) -> u64 {
    items
        .iter()
        .map(|i| estimate_item_with(i.borrow(), profile))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_overcount_not_undercount() {
        // Preserved from M2: ~4 chars/token English must estimate high.
        let text = "the quick brown fox jumps over the lazy dog. ".repeat(100);
        let actual_ish = text.len() as u64 / 4;
        assert!(estimate_text(&text) > actual_ish);

        let items = vec![
            Item::User {
                text: text.clone(),
                synthetic: None,
                images: Vec::new(),
            },
            Item::Assistant {
                blocks: vec![serde_json::json!({"type":"text","text":text})],
            },
        ];
        assert!(estimate_items(&items) > 2 * actual_ish);
    }

    #[test]
    fn cjk_is_counted_by_character_not_by_byte_and_never_undercounts() {
        // 100 CJK chars = 300 UTF-8 bytes, and tokenize to ~100-150 tokens.
        let cjk = "日本語のテキストです".repeat(10); // 100 chars
        assert_eq!(cjk.chars().count(), 100);
        let est = estimate_text(&cjk);
        // The floor is the thing that matters: never below ~1 token/char.
        assert!(
            est >= 100,
            "undercount is the one unacceptable direction: {est}"
        );
        // And not absurdly high — the old bytes/3 answer was 100; a naive
        // chars/3 would have been 34 (the bug this test exists to prevent).
        assert!(est <= 200, "{est}");
    }

    #[test]
    fn emoji_weigh_more_than_one_token_each() {
        // Astral-plane scalars usually tokenize to 2-4 tokens apiece.
        let emoji = "🙂🎉🚀".repeat(10); // 30 astral chars
        assert!(estimate_text(&emoji) >= 60, "{}", estimate_text(&emoji));
    }

    #[test]
    fn images_are_estimated_flat_never_as_base64_text() {
        // A "5MB" payload: if the base64 ever went through text estimation
        // this would be ~1.7M tokens and trip compaction instantly.
        let item = Item::User {
            text: "look: [Image #1]".into(),
            synthetic: None,
            images: vec![hotl_types::UserImage {
                media_type: "image/png".into(),
                data: "A".repeat(5 * 1024 * 1024).into(),
            }],
        };
        let text_only = Item::User {
            text: "look: [Image #1]".into(),
            synthetic: None,
            images: Vec::new(),
        };
        assert_eq!(
            estimate_item(&item),
            estimate_item(&text_only) + IMAGE_TOKENS_ESTIMATE
        );
    }

    #[test]
    fn item_overhead_is_charged_once_per_item() {
        let empty = Item::User {
            text: String::new(),
            synthetic: None,
            images: Vec::new(),
        };
        assert_eq!(estimate_item(&empty), ITEM_OVERHEAD);
        assert_eq!(estimate_items::<Item>(&[]), 0);
        // Two empty items cost exactly two envelopes.
        assert_eq!(estimate_items(&[empty.clone(), empty]), 2 * ITEM_OVERHEAD);
    }

    #[test]
    fn tool_results_charge_a_per_result_envelope() {
        use hotl_types::ToolResultItem;
        let one = Item::ToolResults {
            results: vec![ToolResultItem {
                tool_use_id: "a".into(),
                content: String::new(),
                is_error: false,
            }],
        };
        let two = Item::ToolResults {
            results: vec![
                ToolResultItem {
                    tool_use_id: "a".into(),
                    content: String::new(),
                    is_error: false,
                },
                ToolResultItem {
                    tool_use_id: "b".into(),
                    content: String::new(),
                    is_error: false,
                },
            ],
        };
        assert_eq!(estimate_item(&one), ITEM_OVERHEAD + RESULT_OVERHEAD);
        assert_eq!(estimate_item(&two), ITEM_OVERHEAD + 2 * RESULT_OVERHEAD);
    }

    #[test]
    fn assistant_blocks_do_not_bill_their_json_framing() {
        let body = "x".repeat(3000);
        let blocks = vec![serde_json::json!({"type": "text", "text": body})];
        let est = estimate_item(&Item::Assistant { blocks });
        // Still an overcount of the payload (the invariant)…
        assert!(est > 3000 / 4, "{est}");
        // …but not inflated by `{"type":"text","text":…}` framing: the old
        // implementation serialized the whole block, adding ~30 chars of
        // punctuation plus an escape per special character.
        assert!(
            est < ITEM_OVERHEAD + 3000 / 2,
            "framing is still being billed: {est}"
        );
    }

    #[test]
    fn a_denser_profile_estimates_higher() {
        let text = "hello world, this is ordinary English prose. ".repeat(50);
        let loose = TokenProfile {
            ascii_chars_per_token: 4.0,
            ..TokenProfile::CONSERVATIVE
        };
        let dense = TokenProfile {
            ascii_chars_per_token: 2.5,
            ..TokenProfile::CONSERVATIVE
        };
        assert!(estimate_text_with(&text, &dense) > estimate_text_with(&text, &loose));
        // The zero-arg entry point is the conservative profile, unchanged.
        assert_eq!(
            estimate_text(&text),
            estimate_text_with(&text, &TokenProfile::CONSERVATIVE)
        );
    }

    #[test]
    fn from_chars_per_token_clamps_nonsense() {
        // A catalog row with a bad ratio must not produce a divide-by-zero or
        // a negative estimate.
        assert!(TokenProfile::from_chars_per_token(0.0).ascii_chars_per_token >= 1.0);
        assert!(TokenProfile::from_chars_per_token(-3.0).ascii_chars_per_token >= 1.0);
        assert!(estimate_text_with("abc", &TokenProfile::from_chars_per_token(0.0)) > 0);
    }

    #[test]
    fn image_bytes_counts_base64_payloads_and_ignores_everything_else() {
        let items = vec![
            Item::System { text: "sys".into() },
            Item::User {
                text: "look".into(),
                synthetic: None,
                images: vec![
                    hotl_types::UserImage {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                    hotl_types::UserImage {
                        media_type: "image/png".into(),
                        data: "BBBBBBBB".into(),
                    },
                ],
            },
        ];
        assert_eq!(image_b64_bytes(&items), 12);
        assert_eq!(image_b64_bytes(&items[..1]), 0);
    }
}
