//! Extension hooks (M5). The engine consults a `Hooks` impl at two
//! points in the tool phase — before a call runs and after it returns —
//! scoped to the events actually used, not a 35-event schema (Delivery #5).
//!
//! Vocabulary follows 0006's M5 pin #2: a **wrap-style** `PreToolUse` hook
//! *intercepts* a call (allow / block / transiently rewrite its input); a
//! **node-style** `PostToolUse` hook returns a *proposal* to replace the
//! result. Hook-visible payloads are byte-capped (pin #1) so a hook can't be
//! used to amplify a huge tool result into the process.

use futures_util::future::BoxFuture;
use serde_json::Value;

/// Default cap on the bytes of a tool result a hook is shown.
pub const HOOK_PAYLOAD_CAP: usize = 2048;

/// A `PreToolUse` decision (wrap-style intercept). A `Rewrite` re-enters the
/// normal permission gate with the new input — a hook cannot launder a call
/// past the y/N ask (SECURITY.md §M5 routing rows).
#[derive(Debug, Clone, PartialEq)]
pub enum PreToolDecision {
    Continue,
    Deny { message: String },
    Rewrite { input: Value },
}

pub trait Hooks: Send + Sync {
    /// Before a tool runs. The hook sees the tool name and full input.
    fn pre_tool<'a>(&'a self, name: &'a str, input: &'a Value) -> BoxFuture<'a, PreToolDecision>;
    /// After a tool succeeds. `result` is byte-capped to `HOOK_PAYLOAD_CAP`.
    /// `Some(replacement)` swaps the result the model sees; `None` leaves it.
    fn post_tool<'a>(&'a self, name: &'a str, result: &'a str) -> BoxFuture<'a, Option<String>>;
}

/// Clip a payload to the hook cap on a char boundary (never mid-UTF-8).
pub fn cap_payload(s: &str) -> &str {
    if s.len() <= HOOK_PAYLOAD_CAP {
        return s;
    }
    let mut end = HOOK_PAYLOAD_CAP;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Lane 1 — in-process Rust hooks: the compiled-in registry that the
/// shell adapter (lane 2) and any future third-party lane register against.
#[derive(Default)]
pub struct InProcessHooks {
    #[allow(clippy::type_complexity)]
    pre: Vec<Box<dyn Fn(&str, &Value) -> PreToolDecision + Send + Sync>>,
    #[allow(clippy::type_complexity)]
    post: Vec<Box<dyn Fn(&str, &str) -> Option<String> + Send + Sync>>,
}

impl InProcessHooks {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn on_pre_tool(
        mut self,
        f: impl Fn(&str, &Value) -> PreToolDecision + Send + Sync + 'static,
    ) -> Self {
        self.pre.push(Box::new(f));
        self
    }
    pub fn on_post_tool(
        mut self,
        f: impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.post.push(Box::new(f));
        self
    }
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }
}

impl Hooks for InProcessHooks {
    fn pre_tool<'a>(&'a self, name: &'a str, input: &'a Value) -> BoxFuture<'a, PreToolDecision> {
        Box::pin(async move {
            // First hook to intercept wins (deny/rewrite short-circuits).
            for hook in &self.pre {
                match hook(name, input) {
                    PreToolDecision::Continue => {}
                    decision => return decision,
                }
            }
            PreToolDecision::Continue
        })
    }
    fn post_tool<'a>(&'a self, name: &'a str, result: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let capped = cap_payload(result);
            let mut current: Option<String> = None;
            for hook in &self.post {
                let view = current.as_deref().unwrap_or(capped);
                if let Some(replacement) = hook(name, view) {
                    current = Some(replacement);
                }
            }
            current
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn pre_hook_denies_rewrites_or_continues() {
        let hooks = InProcessHooks::new().on_pre_tool(|name, input| {
            if name == "bash" && input.get("command").and_then(Value::as_str) == Some("rm -rf /") {
                PreToolDecision::Deny { message: "no destructive commands".into() }
            } else if name == "write" {
                PreToolDecision::Rewrite { input: json!({"path": "safe.txt", "content": "x"}) }
            } else {
                PreToolDecision::Continue
            }
        });
        assert_eq!(
            hooks.pre_tool("bash", &json!({"command": "rm -rf /"})).await,
            PreToolDecision::Deny { message: "no destructive commands".into() }
        );
        assert!(matches!(
            hooks.pre_tool("write", &json!({"path": "x", "content": "y"})).await,
            PreToolDecision::Rewrite { .. }
        ));
        assert_eq!(hooks.pre_tool("read", &json!({})).await, PreToolDecision::Continue);
    }

    #[tokio::test]
    async fn post_hook_caps_and_replaces() {
        let hooks = InProcessHooks::new().on_post_tool(|_n, result| {
            Some(format!("[annotated] {} chars", result.len()))
        });
        let big = "z".repeat(HOOK_PAYLOAD_CAP * 2);
        let out = hooks.post_tool("read", &big).await.unwrap();
        // The hook only ever saw the capped payload.
        assert!(out.contains(&format!("{} chars", HOOK_PAYLOAD_CAP)));
    }
}
