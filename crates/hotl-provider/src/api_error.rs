//! Turning an HTTP error body into a line a human can act on.
//!
//! Providers answer failures with JSON, and the shapes differ: Anthropic and
//! OpenAI nest the text under `error.message`, AWS-style endpoints put it at
//! `message` with the class in an `x-amzn-errortype` header, and gateways
//! invent their own. Surfacing the raw body means the reader has to find the
//! one sentence that matters inside a blob of JSON — so that sentence is
//! pulled out here, and the body is only shown verbatim when nothing else can
//! be made of it.

use serde_json::Value;

/// Longest message kept before clipping — enough for a full API sentence,
/// short enough to read in a transcript notice.
const MAX_MESSAGE: usize = 400;

/// What went wrong, as `<class>: <what>` — the detail a `ProviderError` carries
/// and its `Display` prefixes with the status. `class` comes from the body's
/// error type or the provider's error-type header, and is dropped when neither
/// says anything useful.
pub fn detail(error_type: Option<&str>, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let class = parsed
        .as_ref()
        .and_then(class_of)
        .or_else(|| error_type.map(short_type))
        .filter(|c| !c.is_empty());
    let message = parsed
        .as_ref()
        .and_then(message_of)
        .unwrap_or_else(|| one_line(body));
    let message = clip(&message);
    match (class, message.is_empty()) {
        (Some(class), false) => format!("{class}: {message}"),
        (Some(class), true) => class,
        (None, false) => message,
        (None, true) => "the response carried no error details".into(),
    }
}

/// The human sentence, wherever this provider keeps it.
fn message_of(body: &Value) -> Option<String> {
    let candidates = [
        body.pointer("/error/message"),
        body.pointer("/message"),
        body.pointer("/error"),
        body.pointer("/error_description"),
        body.pointer("/detail"),
    ];
    candidates
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        .map(one_line)
        .filter(|m| !m.is_empty())
}

/// The error class: `invalid_request_error`, `ValidationException`, …
fn class_of(body: &Value) -> Option<String> {
    let candidates = [
        body.pointer("/error/type"),
        body.pointer("/error/code"),
        body.pointer("/__type"),
        body.pointer("/type"),
    ];
    candidates
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        // Anthropic wraps every error body in {"type":"error"} — that names
        // the envelope, not the failure.
        .filter(|t| *t != "error")
        .map(short_type)
}

/// AWS sends `com.amazon.coral.service#ValidationException`; keep the tail.
fn short_type(t: &str) -> String {
    t.rsplit(['#', '.']).next().unwrap_or(t).trim().to_string()
}

/// Collapse to a single line — a transcript notice gets one.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_MESSAGE {
        return s.to_string();
    }
    let kept: String = s.chars().take(MAX_MESSAGE).collect();
    format!("{kept}… (truncated)")
}

#[cfg(test)]
mod tests {
    use super::detail;
    use crate::ProviderError;

    #[test]
    fn anthropic_style_bodies_lose_the_envelope() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error",
            "message":"messages.4: tool_use ids were found without tool_result blocks"}}"#;
        assert_eq!(
            detail(None, body),
            "invalid_request_error: messages.4: tool_use ids were found \
             without tool_result blocks"
        );
    }

    #[test]
    fn aws_style_bodies_use_the_header_class() {
        let body = concat!(
            r#"{"message":"The number of toolResult blocks at messages.4.content "#,
            r#"exceeds the number of toolUse blocks of previous turn."}"#
        );
        assert_eq!(
            detail(Some("com.amazon.coral.service#ValidationException"), body),
            "ValidationException: The number of toolResult blocks at \
             messages.4.content exceeds the number of toolUse blocks of previous turn."
        );
    }

    #[test]
    fn openai_style_bodies_read_the_same_way() {
        let body = r#"{"error":{"message":"Rate limit reached","type":"rate_limit_error",
            "code":"rate_limit_exceeded"}}"#;
        assert_eq!(detail(None, body), "rate_limit_error: Rate limit reached");
    }

    #[test]
    fn a_bare_string_error_still_reads() {
        assert_eq!(
            detail(None, r#"{"error":"upstream unavailable"}"#),
            "upstream unavailable"
        );
    }

    #[test]
    fn html_and_other_non_json_survive_as_one_line() {
        let body = "<html>\n  <body>502 Bad Gateway</body>\n</html>";
        assert_eq!(
            detail(None, body),
            "<html> <body>502 Bad Gateway</body> </html>"
        );
    }

    #[test]
    fn an_empty_body_still_says_something() {
        assert_eq!(detail(None, ""), "the response carried no error details");
        assert_eq!(detail(Some("ServiceUnavailable"), ""), "ServiceUnavailable");
    }

    #[test]
    fn a_runaway_body_is_clipped_not_dumped() {
        let body = format!(r#"{{"message":"{}"}}"#, "x".repeat(5_000));
        let described = detail(None, &body);
        assert!(described.ends_with("… (truncated)"));
        assert!(described.chars().count() < 500, "{described}");
    }

    /// The status belongs to the error's own Display — the detail must not
    /// repeat it, or the reader gets `HTTP 400: HTTP 400 ...`.
    #[test]
    fn the_rendered_error_names_the_status_exactly_once() {
        let rendered = ProviderError::Http {
            status: 400,
            message: detail(Some("ValidationException"), r#"{"message":"bad request"}"#),
            retry_after: None,
        }
        .to_string();
        assert_eq!(rendered, "HTTP 400: ValidationException: bad request");
        assert_eq!(rendered.matches("HTTP 400").count(), 1);
    }
}
