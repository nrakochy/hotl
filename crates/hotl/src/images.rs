//! Wire-side validation for user-attached images.
//!
//! Validation happens HERE, at the entry points (`acp.rs`, `session_server.rs`),
//! before anything reaches the engine: a poisoned base64 item is *durable* —
//! once committed to the log, every subsequent sample re-sends it and 400s at
//! the provider until compaction folds it, effectively bricking the session.
//! Reject at the wire, before the log.
//!
//! The shape check is hand-rolled (~25 lines): the providers ship the encoded
//! string verbatim, so a full decode buys nothing, and the workspace
//! deliberately has no base64 dependency (`hotl_tools::b64` is encode-only).

use hotl_types::UserImage;
use serde_json::Value;

/// Per-prompt attachment cap. Well under the API's own limits; a prompt that
/// wants more is better served by paths and the read tool.
pub const MAX_IMAGES_PER_PROMPT: usize = 8;

/// Per-image ceiling on DECODED bytes — Anthropic's per-image API limit, the
/// strictest mainstream provider. The TUI runtime enforces the same constant
/// client-side (`tui.rs`), so a well-behaved client never trips this.
pub const MAX_IMAGE_DECODED_BYTES: usize = 5 * 1024 * 1024;

/// Per-prompt ceiling on total decoded bytes: headroom under the ~32MB
/// request cap once history and tool schemas ride along.
pub const MAX_PROMPT_DECODED_BYTES: usize = 16 * 1024 * 1024;

const MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Read an optional `images` array off `params` (`{media_type, data}`
/// objects). Absent or `null` → `Ok(empty)`. Every error string names what
/// to fix — errors are prompts here too, they go back to a client.
pub fn parse_images(params: &Value) -> Result<Vec<UserImage>, String> {
    let arr = match params.get("images") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(v) => v
            .as_array()
            .ok_or("images must be an array of {media_type, data} objects")?,
    };
    if arr.len() > MAX_IMAGES_PER_PROMPT {
        return Err(format!(
            "{} images exceeds the {MAX_IMAGES_PER_PROMPT}-per-prompt cap",
            arr.len()
        ));
    }
    let mut total = 0usize;
    let mut out = Vec::with_capacity(arr.len());
    for (i, img) in arr.iter().enumerate() {
        let media_type = img
            .get("media_type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("images[{i}] is missing media_type"))?;
        if !MEDIA_TYPES.contains(&media_type) {
            return Err(format!(
                "images[{i}] media_type {media_type:?} is not one of {MEDIA_TYPES:?}"
            ));
        }
        let data = img
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("images[{i}] is missing data"))?;
        let decoded = decoded_len(data).map_err(|e| format!("images[{i}]: {e}"))?;
        if decoded > MAX_IMAGE_DECODED_BYTES {
            return Err(format!(
                "images[{i}] is {:.1}MB decoded — the per-image cap is {}MB",
                decoded as f64 / (1024.0 * 1024.0),
                MAX_IMAGE_DECODED_BYTES / (1024 * 1024),
            ));
        }
        total += decoded;
        if total > MAX_PROMPT_DECODED_BYTES {
            return Err(format!(
                "images total more than the {}MB per-prompt cap",
                MAX_PROMPT_DECODED_BYTES / (1024 * 1024),
            ));
        }
        out.push(UserImage {
            media_type: media_type.to_string(),
            data: data.into(),
        });
    }
    Ok(out)
}

/// Decoded size of a well-formed RFC 4648 string (standard alphabet, padded),
/// or what is wrong with it. Shape-only — no allocation, no decode.
fn decoded_len(data: &str) -> Result<usize, String> {
    if data.starts_with("data:") {
        return Err("data must be bare base64, not a data: URL".into());
    }
    if data.is_empty() || !data.len().is_multiple_of(4) {
        return Err("data must be non-empty base64 with padding (length % 4 == 0)".into());
    }
    let bytes = data.as_bytes();
    let pad = bytes.iter().rev().take_while(|&&b| b == b'=').count();
    if pad > 2 {
        return Err("data has more than two '=' padding chars".into());
    }
    let body = &bytes[..bytes.len() - pad];
    if let Some(bad) = body
        .iter()
        .find(|&&b| !(b.is_ascii_alphanumeric() || b == b'+' || b == b'/'))
    {
        return Err(format!(
            "data contains a non-base64 character ({:?})",
            *bad as char
        ));
    }
    Ok(data.len() / 4 * 3 - pad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn img(media_type: &str, data: &str) -> Value {
        json!({"images": [{"media_type": media_type, "data": data}]})
    }

    #[test]
    fn absent_or_null_images_parse_to_empty() {
        assert_eq!(parse_images(&json!({"text": "hi"})), Ok(Vec::new()));
        assert_eq!(parse_images(&json!({"images": null})), Ok(Vec::new()));
    }

    #[test]
    fn a_valid_image_round_trips() {
        let got = parse_images(&img("image/png", "aW1nMQ==")).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].media_type, "image/png");
        assert_eq!(&*got[0].data, "aW1nMQ==");
    }

    #[test]
    fn bad_shapes_are_rejected_with_a_reason() {
        for (params, needle) in [
            (json!({"images": "nope"}), "must be an array"),
            (img("image/tiff", "aW1n"), "not one of"),
            (json!({"images": [{"data": "aW1n"}]}), "missing media_type"),
            (
                json!({"images": [{"media_type": "image/png"}]}),
                "missing data",
            ),
            (img("image/png", "data:image/png;base64,aW1n"), "data: URL"),
            (img("image/png", "abc"), "length % 4"),
            (img("image/png", ""), "non-empty"),
            (img("image/png", "aW!n"), "non-base64 character"),
            (img("image/png", "a==="), "more than two"),
        ] {
            let err = parse_images(&params).unwrap_err();
            assert!(err.contains(needle), "{params} -> {err}");
        }
    }

    #[test]
    fn caps_are_enforced_on_count_and_decoded_size() {
        let many: Vec<Value> = (0..MAX_IMAGES_PER_PROMPT + 1)
            .map(|_| json!({"media_type": "image/png", "data": "aW1n"}))
            .collect();
        let err = parse_images(&json!({"images": many})).unwrap_err();
        assert!(err.contains("per-prompt cap"), "{err}");

        // A base64 string whose decoded size exceeds the per-image cap; the
        // charset check sees only 'A's, so this stays cheap to construct.
        let big = "A".repeat((MAX_IMAGE_DECODED_BYTES / 3 + 1) * 4);
        let err = parse_images(&img("image/png", &big)).unwrap_err();
        assert!(err.contains("per-image cap"), "{err}");
    }

    #[test]
    fn decoded_len_matches_the_padding_math() {
        assert_eq!(decoded_len("aW1n"), Ok(3));
        assert_eq!(decoded_len("aW1nMQ=="), Ok(4));
        assert_eq!(decoded_len("aW1nMTI="), Ok(5));
    }
}
