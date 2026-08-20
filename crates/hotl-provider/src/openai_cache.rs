//! GPT-5.6+ caches at explicit breakpoints and bills writes at 1.25×; a
//! trailing per-sample user message (hotl's MOIM) under the default implicit
//! breakpoint rewrites the whole prefix every sample. Shared by both OpenAI
//! dialects so they cannot drift (plan 0046).

use crate::ProviderError;
use serde_json::{json, Value};

/// Does the model name carry `gpt-<major>[.<minor>]` at or past 5.6? Fails
/// closed: a marker a server rejects is a 400, implicit caching is only slow.
pub fn breakpoints_supported(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let Some(pos) = lower.find("gpt-") else {
        return false;
    };
    let rest = &lower[pos + 4..];
    let digits = |s: &str| {
        s.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
    };
    let major = digits(rest);
    if major.is_empty() {
        return false;
    }
    let after = &rest[major.len()..];
    let minor = after.strip_prefix('.').map(digits).unwrap_or_default();
    let major: u32 = major.parse().unwrap_or(0);
    let minor: u32 = minor.parse().unwrap_or(0);
    (major, minor) >= (5, 6)
}

/// The block-level marker. `explicit` is the only valid mode.
pub fn breakpoint() -> Value {
    json!({"mode": "explicit"})
}

/// The request-level policy: explicit-only, no `ttl` (30m is the only value
/// and the default).
pub fn options() -> Value {
    json!({"mode": "explicit"})
}

/// A 400/422 whose structured message names a prompt-cache field: the endpoint
/// (or a gateway in front of it) predates breakpoints. Twin of the chat
/// dialect's `rejects_max_completion_tokens`.
pub fn rejects_breakpoints(err: &ProviderError) -> bool {
    matches!(
        err,
        ProviderError::Http { status, message, .. }
            if (*status == 400 || *status == 422) && message.contains("prompt_cache")
    )
}

/// OpenAI's prompt total *includes* cached and written tokens; `TokenUsage`
/// counts both outside `input_tokens` (Anthropic semantics).
/// Returns `(input, cache_read, cache_creation)`.
pub fn carve_usage(total: u64, cached: u64, written: u64) -> (u64, u64, u64) {
    (
        total.saturating_sub(cached).saturating_sub(written),
        cached,
        written,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_gate_opens_at_gpt_5_6_and_fails_closed_elsewhere() {
        for on in [
            "gpt-5.6",
            "gpt-5.6-sol",
            "gpt-5.6-luna-1",
            "gpt-5.7-mini",
            "gpt-5.10",
            "gpt-6",
            "GPT-5.6",
            "openai/gpt-5.6",
        ] {
            assert!(breakpoints_supported(on), "{on} should support breakpoints");
        }
        for off in [
            "gpt-5.5", "gpt-5", "gpt-4.1", "gpt-4o", "o3", "llama3.1", "sol-fast", "gpt-", "",
        ] {
            assert!(!breakpoints_supported(off), "{off} should fail closed");
        }
    }

    #[test]
    fn the_guides_own_example_carves_into_input_read_and_written() {
        assert_eq!(carve_usage(2600, 2000, 400), (200, 2000, 400));
        assert_eq!(carve_usage(100, 0, 0), (100, 0, 0));
        // Saturates instead of wrapping when a gateway over-reports.
        assert_eq!(carve_usage(100, 80, 50), (0, 80, 50));
    }

    #[test]
    fn the_probe_matches_only_a_structured_prompt_cache_4xx() {
        let http = |status: u16, message: &str| ProviderError::Http {
            status,
            message: message.into(),
            retry_after: None,
        };
        assert!(rejects_breakpoints(&http(
            400,
            "Unknown parameter: 'prompt_cache_options'."
        )));
        assert!(rejects_breakpoints(&http(
            422,
            "input[0].content[0].prompt_cache_breakpoint is not permitted"
        )));
        assert!(!rejects_breakpoints(&http(400, "context length exceeded")));
        assert!(!rejects_breakpoints(&http(
            500,
            "prompt_cache_options blew up"
        )));
        assert!(!rejects_breakpoints(&ProviderError::Transport(
            "prompt_cache".into()
        )));
    }

    #[test]
    fn the_marker_and_the_options_are_explicit_only() {
        assert_eq!(breakpoint(), json!({"mode": "explicit"}));
        assert_eq!(options(), json!({"mode": "explicit"}));
    }
}
