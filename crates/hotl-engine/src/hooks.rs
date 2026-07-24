//! Extension hooks (M5). The engine consults a `Hooks` impl at two
//! points in the tool phase — before a call runs and after it returns —
//! scoped to the events actually used, not a 35-event schema (Delivery #5).
//!
//! Vocabulary follows 0006's M5 pin #2: a **wrap-style** `PreToolUse` hook
//! *intercepts* a call (allow / block / transiently rewrite its input); a
//! **node-style** `PostToolUse` hook returns a *proposal* to replace the
//! result. Hook-visible payloads are byte-capped (pin #1) so a hook can't be
//! used to amplify a huge tool result into the process.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;

/// Default cap on the bytes of a tool result a hook is shown.
pub const HOOK_PAYLOAD_CAP: usize = 2048;

/// Wall-clock budget for a *blocking* hook call (`on_user_prompt`, `on_stop`)
/// — the turn awaits these because they return context/decisions it needs.
/// A hook that exceeds this is treated as a no-op (never a grant, never a
/// block), so a crashed or hung hook can't stall a turn indefinitely. Shell
/// hooks already bound each subprocess invocation more tightly
/// (`shell_hooks::HOOK_TIMEOUT_SECS`); this is the outer safety net that
/// applies to every `Hooks` impl, including a future non-subprocess lane.
pub const HOOK_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Await [`Hooks::on_user_prompt`] under [`HOOK_CALL_TIMEOUT`]; a timeout
/// behaves exactly like `None` — no context, never a crash.
pub async fn call_user_prompt(hooks: &Arc<dyn Hooks>, prompt: &str) -> Option<String> {
    tokio::time::timeout(HOOK_CALL_TIMEOUT, hooks.on_user_prompt(prompt))
        .await
        .ok()
        .flatten()
}

/// Budget for a background (`on_notification`/`on_session_end`) hook call —
/// generous (it never blocks anything), but bounded, so a "phone home" hook
/// can't leak a task forever.
pub const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(10);

/// `Notification` (tier-1 gap #7, the `hotl watch`/desktop seam): spawn
/// `on_notification` **detached**, under its own timeout, and return
/// immediately — the caller MUST NOT `.await` this. A 2s (or hung) notifier
/// must never stall the turn or the actor loop that calls it (Concurrency &
/// parallelism §"Background (fire-and-forget) hooks").
pub fn notify(hooks: &Arc<dyn Hooks>, kind: NotificationKind, detail: impl Into<String>) {
    let hooks = Arc::clone(hooks);
    let detail = detail.into();
    tokio::spawn(async move {
        let _ =
            tokio::time::timeout(NOTIFICATION_TIMEOUT, hooks.on_notification(kind, &detail)).await;
    });
}

/// Await [`Hooks::on_stop`] under [`HOOK_CALL_TIMEOUT`]; a timeout behaves
/// exactly like `Allow` — a hung hook can never wedge a turn (it's a no-op,
/// not a block).
pub async fn call_stop(hooks: &Arc<dyn Hooks>, outcome: &str) -> StopDecision {
    tokio::time::timeout(HOOK_CALL_TIMEOUT, hooks.on_stop(outcome))
        .await
        .unwrap_or(StopDecision::Allow)
}

/// `SessionEnd`: fire-and-forget at actor shutdown, the same detached shape
/// as [`notify`].
pub fn spawn_session_end(hooks: &Arc<dyn Hooks>) {
    let hooks = Arc::clone(hooks);
    tokio::spawn(async move {
        let _ = tokio::time::timeout(NOTIFICATION_TIMEOUT, hooks.on_session_end()).await;
    });
}

/// A `PreToolUse` decision (wrap-style intercept). A `Rewrite` re-enters the
/// normal permission gate with the new input — a hook cannot launder a call
/// past the y/N ask (SECURITY.md §M5 routing rows).
#[derive(Debug, Clone, PartialEq)]
pub enum PreToolDecision {
    Continue,
    Deny { message: String },
    Rewrite { input: Value },
}

/// Per-tool matcher (12 §"tool events match tool name; regex when
/// non-alphanumeric" — hotl deliberately adopts only the exact-name half:
/// full regex is YAGNI for a personal harness). `All` fires for every tool
/// (the pre-matcher default); `Names` fires only for an exact (case-sensitive)
/// name match.
#[derive(Debug, Clone, PartialEq)]
pub enum Matcher {
    All,
    Names(Vec<String>),
}

impl Matcher {
    pub fn matches(&self, tool: &str) -> bool {
        match self {
            Matcher::All => true,
            Matcher::Names(names) => names.iter().any(|n| n == tool),
        }
    }
}

pub trait Hooks: Send + Sync {
    /// Before a tool runs. The hook sees the tool name and full input.
    fn pre_tool<'a>(&'a self, name: &'a str, input: &'a Value) -> BoxFuture<'a, PreToolDecision>;
    /// After a tool succeeds. `result` is byte-capped to `HOOK_PAYLOAD_CAP`.
    /// `Some(replacement)` swaps the result the model sees; `None` leaves it.
    fn post_tool<'a>(&'a self, name: &'a str, result: &'a str) -> BoxFuture<'a, Option<String>>;

    /// `UserPromptSubmit`: runs when a prompt is admitted, before the turn it
    /// starts samples. `Some(context)` becomes one `SyntheticReason::SystemReminder`
    /// user item committed right after the prompt — a tagged user item, never
    /// a system-prompt edit (prefix-cache stability). Default: no-op (`None`),
    /// so a lane that hasn't wired this event compiles and behaves inertly.
    fn on_user_prompt<'a>(&'a self, _prompt: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(std::future::ready(None))
    }

    /// `Notification`: fire-and-forget — the engine tells hooks the agent
    /// blocked on a human (`Blocked`), went idle (`Idle`), or finished
    /// (`Done`). Callers MUST NOT await this on the hot path (see
    /// `spawn_notification`); the default is a no-op.
    fn on_notification<'a>(
        &'a self,
        _kind: NotificationKind,
        _detail: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }

    /// `Stop`: a bounded veto at the turn's Done branch (tech-debt #10,
    /// node-style: it returns a decision, it doesn't wrap the branch).
    /// Default: `Allow` — a hook-less build never delays turn-end.
    fn on_stop<'a>(&'a self, _outcome: &'a str) -> BoxFuture<'a, StopDecision> {
        Box::pin(std::future::ready(StopDecision::Allow))
    }

    /// `SessionEnd`: fire-and-forget, called once at actor shutdown. Default:
    /// no-op.
    fn on_session_end<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }
}

/// The `Notification` hook's kind (tier-1 gap #7, the `hotl watch`/desktop
/// seam): the agent blocked on a human, went idle awaiting a prompt, or
/// completed a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    Blocked,
    Idle,
    Done,
}

/// A `Stop` hook's veto decision (tech-debt #10's node-vs-wrap pin: `Stop` is
/// node-style — it returns a decision, never wraps the branch itself).
#[derive(Debug, Clone, PartialEq)]
pub enum StopDecision {
    Allow,
    Block { reason: String },
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
    pre: Vec<(
        Matcher,
        Box<dyn Fn(&str, &Value) -> PreToolDecision + Send + Sync>,
    )>,
    #[allow(clippy::type_complexity)]
    post: Vec<(
        Matcher,
        Box<dyn Fn(&str, &str) -> Option<String> + Send + Sync>,
    )>,
    #[allow(clippy::type_complexity)]
    prompt: Vec<Box<dyn Fn(&str) -> Option<String> + Send + Sync>>,
    #[allow(clippy::type_complexity)]
    notification: Vec<Box<dyn Fn(NotificationKind, &str) + Send + Sync>>,
    #[allow(clippy::type_complexity)]
    stop: Vec<Box<dyn Fn(&str) -> StopDecision + Send + Sync>>,
    #[allow(clippy::type_complexity)]
    session_end: Vec<Box<dyn Fn() + Send + Sync>>,
}

impl InProcessHooks {
    pub fn new() -> Self {
        Self::default()
    }
    /// `Matcher::All` sugar — fires on every tool (back-compat shape).
    pub fn on_pre_tool(
        self,
        f: impl Fn(&str, &Value) -> PreToolDecision + Send + Sync + 'static,
    ) -> Self {
        self.on_pre_tool_matching(Matcher::All, f)
    }
    pub fn on_pre_tool_matching(
        mut self,
        matcher: Matcher,
        f: impl Fn(&str, &Value) -> PreToolDecision + Send + Sync + 'static,
    ) -> Self {
        self.pre.push((matcher, Box::new(f)));
        self
    }
    /// `Matcher::All` sugar — fires on every tool (back-compat shape).
    pub fn on_post_tool(
        self,
        f: impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.on_post_tool_matching(Matcher::All, f)
    }
    pub fn on_post_tool_matching(
        mut self,
        matcher: Matcher,
        f: impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.post.push((matcher, Box::new(f)));
        self
    }
    pub fn on_user_prompt(
        mut self,
        f: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.prompt.push(Box::new(f));
        self
    }
    pub fn on_notification(
        mut self,
        f: impl Fn(NotificationKind, &str) + Send + Sync + 'static,
    ) -> Self {
        self.notification.push(Box::new(f));
        self
    }
    pub fn on_stop(mut self, f: impl Fn(&str) -> StopDecision + Send + Sync + 'static) -> Self {
        self.stop.push(Box::new(f));
        self
    }
    pub fn on_session_end(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.session_end.push(Box::new(f));
        self
    }
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty()
            && self.post.is_empty()
            && self.prompt.is_empty()
            && self.notification.is_empty()
            && self.stop.is_empty()
            && self.session_end.is_empty()
    }
}

/// Deterministic most-restrictive merge over `pre_tool` results collected
/// from every matching hook (Innovation #1): `Deny` beats `Rewrite` beats
/// `Continue`. `results` is in **registration order** (the order the
/// matching hooks were folded, not the order their futures completed —
/// `join_all` preserves input order regardless of completion order), so a
/// tie among same-severity decisions always resolves to the
/// first-registered hook, never a race. Exposed (not just used internally by
/// `InProcessHooks`) so lane 2 (the shell adapter) shares the exact same
/// merge discipline instead of re-implementing it.
pub fn merge_pre_tool(results: Vec<PreToolDecision>) -> PreToolDecision {
    if let Some(deny) = results
        .iter()
        .find(|d| matches!(d, PreToolDecision::Deny { .. }))
    {
        return deny.clone();
    }
    if let Some(rewrite) = results
        .iter()
        .find(|d| matches!(d, PreToolDecision::Rewrite { .. }))
    {
        return rewrite.clone();
    }
    PreToolDecision::Continue
}

/// Deterministic most-restrictive merge over `Stop` results: any `Block`
/// wins, first-registered among ties — the same discipline as
/// [`merge_pre_tool`], exposed for the shell adapter to share.
pub fn merge_stop(results: Vec<StopDecision>) -> StopDecision {
    results
        .into_iter()
        .find(|d| matches!(d, StopDecision::Block { .. }))
        .unwrap_or(StopDecision::Allow)
}

impl Hooks for InProcessHooks {
    fn pre_tool<'a>(&'a self, name: &'a str, input: &'a Value) -> BoxFuture<'a, PreToolDecision> {
        Box::pin(async move {
            let futures = self
                .pre
                .iter()
                .filter(|(matcher, _)| matcher.matches(name))
                .map(|(_, hook)| async move { hook(name, input) });
            merge_pre_tool(futures_util::future::join_all(futures).await)
        })
    }
    fn post_tool<'a>(&'a self, name: &'a str, result: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let capped = cap_payload(result);
            let mut current: Option<String> = None;
            // Node-style proposal chain: each matching hook sees the
            // previous one's replacement (not a race — a later hook is
            // meant to refine an earlier one's output), so this stays
            // sequential rather than joined.
            for (matcher, hook) in &self.post {
                if !matcher.matches(name) {
                    continue;
                }
                let view = current.as_deref().unwrap_or(capped);
                if let Some(replacement) = hook(name, view) {
                    current = Some(replacement);
                }
            }
            current
        })
    }
    fn on_user_prompt<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let futures = self.prompt.iter().map(|hook| async move { hook(prompt) });
            let results = futures_util::future::join_all(futures).await;
            join_additional_context(results.into_iter().flatten())
        })
    }
    fn on_notification<'a>(&'a self, kind: NotificationKind, detail: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let futures = self
                .notification
                .iter()
                .map(|hook| async move { hook(kind, detail) });
            futures_util::future::join_all(futures).await;
        })
    }
    fn on_stop<'a>(&'a self, outcome: &'a str) -> BoxFuture<'a, StopDecision> {
        Box::pin(async move {
            let futures = self.stop.iter().map(|hook| async move { hook(outcome) });
            merge_stop(futures_util::future::join_all(futures).await)
        })
    }
    fn on_session_end<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let futures = self.session_end.iter().map(|hook| async move { hook() });
            futures_util::future::join_all(futures).await;
        })
    }
}

/// One `additionalContext` item per commit point (Innovation #7): concatenate
/// every non-empty hook result into a single string, capped to the Claude
/// schema's ~10K-char shape, or `None` if nothing was returned.
pub const ADDITIONAL_CONTEXT_CAP: usize = 10_000;

pub fn join_additional_context(parts: impl Iterator<Item = String>) -> Option<String> {
    let mut combined = String::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&part);
    }
    if combined.is_empty() {
        None
    } else {
        Some(cap_str(&combined, ADDITIONAL_CONTEXT_CAP).to_string())
    }
}

/// Clip a string to `max` bytes on a char boundary (never mid-UTF-8) — the
/// same discipline as [`cap_payload`], parameterized for a different cap.
fn cap_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn pre_hook_denies_rewrites_or_continues() {
        let hooks = InProcessHooks::new().on_pre_tool(|name, input| {
            if name == "bash" && input.get("command").and_then(Value::as_str) == Some("rm -rf /") {
                PreToolDecision::Deny {
                    message: "no destructive commands".into(),
                }
            } else if name == "write" {
                PreToolDecision::Rewrite {
                    input: json!({"path": "safe.txt", "content": "x"}),
                }
            } else {
                PreToolDecision::Continue
            }
        });
        assert_eq!(
            hooks
                .pre_tool("bash", &json!({"command": "rm -rf /"}))
                .await,
            PreToolDecision::Deny {
                message: "no destructive commands".into()
            }
        );
        assert!(matches!(
            hooks
                .pre_tool("write", &json!({"path": "x", "content": "y"}))
                .await,
            PreToolDecision::Rewrite { .. }
        ));
        assert_eq!(
            hooks.pre_tool("read", &json!({})).await,
            PreToolDecision::Continue
        );
    }

    #[tokio::test]
    async fn post_hook_caps_and_replaces() {
        let hooks = InProcessHooks::new()
            .on_post_tool(|_n, result| Some(format!("[annotated] {} chars", result.len())));
        let big = "z".repeat(HOOK_PAYLOAD_CAP * 2);
        let out = hooks.post_tool("read", &big).await.unwrap();
        // The hook only ever saw the capped payload.
        assert!(out.contains(&format!("{} chars", HOOK_PAYLOAD_CAP)));
    }

    #[tokio::test]
    async fn matcher_scopes_pre_hook_to_named_tools() {
        let hooks = InProcessHooks::new().on_pre_tool_matching(
            Matcher::Names(vec!["bash".into()]),
            |_n, _i| PreToolDecision::Deny {
                message: "no bash".into(),
            },
        );
        // fires on bash
        assert!(matches!(
            hooks.pre_tool("bash", &json!({})).await,
            PreToolDecision::Deny { .. }
        ));
        // does not fire on read
        assert_eq!(
            hooks.pre_tool("read", &json!({})).await,
            PreToolDecision::Continue
        );
    }

    #[test]
    fn matcher_all_and_names() {
        assert!(Matcher::All.matches("anything"));
        assert!(Matcher::Names(vec!["bash".into(), "write".into()]).matches("write"));
        assert!(!Matcher::Names(vec!["bash".into()]).matches("read"));
    }

    #[tokio::test]
    async fn multiple_matching_pre_hooks_merge_most_restrictive_first() {
        // Registration order: Continue, Rewrite, Deny. Deny must win even
        // though it's registered last — most-restrictive-first, not
        // first-registered-wins.
        let hooks = InProcessHooks::new()
            .on_pre_tool(|_n, _i| PreToolDecision::Continue)
            .on_pre_tool(|_n, _i| PreToolDecision::Rewrite {
                input: json!({"rewritten": true}),
            })
            .on_pre_tool(|_n, _i| PreToolDecision::Deny {
                message: "blocked".into(),
            });
        assert_eq!(
            hooks.pre_tool("bash", &json!({})).await,
            PreToolDecision::Deny {
                message: "blocked".into()
            }
        );
    }

    #[tokio::test]
    async fn ties_among_same_severity_decisions_resolve_by_registration_order() {
        // Two Deny hooks: the FIRST registered must win, regardless of which
        // future would complete first — the fold is over registration
        // order, never completion order (no fast-hook-wins race).
        let hooks = InProcessHooks::new()
            .on_pre_tool(|_n, _i| PreToolDecision::Deny {
                message: "first".into(),
            })
            .on_pre_tool(|_n, _i| PreToolDecision::Deny {
                message: "second".into(),
            });
        assert_eq!(
            hooks.pre_tool("bash", &json!({})).await,
            PreToolDecision::Deny {
                message: "first".into()
            }
        );
    }

    #[tokio::test]
    async fn a_non_matching_hook_never_contributes_to_the_merge() {
        let hooks = InProcessHooks::new()
            .on_pre_tool_matching(Matcher::Names(vec!["write".into()]), |_n, _i| {
                PreToolDecision::Deny {
                    message: "no write".into(),
                }
            })
            .on_pre_tool(|_n, _i| PreToolDecision::Continue);
        // `bash` only matches the `All` hook (Continue) — the `write`-only
        // Deny must not leak into an unrelated tool's decision.
        assert_eq!(
            hooks.pre_tool("bash", &json!({})).await,
            PreToolDecision::Continue
        );
    }
}
