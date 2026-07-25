//! Extension hooks (M5). The engine consults a `Hooks` impl at two
//! points in the tool phase — before a call runs and after it returns —
//! scoped to the events actually used, not a 35-event schema (Delivery #5).
//!
//! Vocabulary follows 0006's M5 pin #2: a **wrap-style** `PreToolUse` hook
//! *intercepts* a call (allow / block / transiently rewrite its input); a
//! **node-style** `PostToolUse` hook returns a *proposal* to replace the
//! result. Hook-visible payloads are byte-capped (pin #1) so a hook can't be
//! used to amplify a huge tool result into the process.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Default cap on the bytes of a tool result a hook is shown.
pub const HOOK_PAYLOAD_CAP: usize = 2048;

/// Appended to a field [`cap_tool_input`] clipped, so a hook can tell a capped
/// view from a genuinely short value. Fixed text, never interpolated: a hook
/// that echoes the view back must produce bytes [`restore_capped`] recognizes.
pub const CAP_MARK: &str = "\n<capped — the tool receives the full value>";

/// Wall-clock budget for a *blocking* hook call — the engine awaits these
/// because they return context or decisions it needs. A hook that exceeds it is
/// treated as a no-op (never a grant, never a block), so a crashed or hung hook
/// can't stall a turn indefinitely. Shell hooks bound each subprocess more
/// tightly (`shell_hooks::HOOK_TIMEOUT_SECS`); this is the outer net.
///
/// INVARIANT: every `Hooks` event the engine awaits is bounded by this AND
/// raced against the turn's cancellation token — including `pre_tool` and
/// `post_tool`, which reach the engine only through [`call_pre_tool`] /
/// [`call_post_tool`]. Enforced by
/// `a_hung_pre_tool_hook_times_out_instead_of_wedging_the_turn`,
/// `a_cancelled_turn_does_not_wait_out_a_hook_timeout`, and
/// `an_interrupt_lands_while_a_pre_tool_hook_hangs`.
pub const HOOK_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// `PreToolUse` under the timeout and the turn's cancel token. Both fall back
/// to `Continue`: a no-op is not a grant — the call still faces the full
/// permission gate (deny rules, protected paths, the human).
pub async fn call_pre_tool(
    hooks: &Arc<dyn Hooks>,
    name: &str,
    input: &Value,
    cancel: &CancellationToken,
) -> PreToolDecision {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => PreToolDecision::Continue,
        decision = tokio::time::timeout(HOOK_CALL_TIMEOUT, hooks.pre_tool(name, input)) => {
            decision.unwrap_or(PreToolDecision::Continue)
        }
    }
}

/// `PostToolUse` under the timeout and the cancel token, with the payload cap
/// applied HERE (pin #1) rather than trusted to each impl. Both fallbacks are
/// `None`: on a timeout or an interrupt the model sees the tool's real output,
/// never a half-applied proposal.
pub async fn call_post_tool(
    hooks: &Arc<dyn Hooks>,
    name: &str,
    result: &str,
    cancel: &CancellationToken,
) -> Option<String> {
    let capped = cap_payload(result);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        replacement = tokio::time::timeout(HOOK_CALL_TIMEOUT, hooks.post_tool(name, capped)) => {
            replacement.ok().flatten()
        }
    }
}

/// The capped rendering of one field, or `None` when it already fits.
fn capped_view(s: &str) -> Option<String> {
    (s.len() > HOOK_PAYLOAD_CAP).then(|| format!("{}{CAP_MARK}", cap_payload(s)))
}

/// The capped hook *view* of a tool input: every top-level string field over
/// [`HOOK_PAYLOAD_CAP`] is clipped and marked. Nested values are left alone —
/// tool inputs are flat by convention, and a hook that needs more should read
/// the file itself.
pub fn cap_tool_input(input: &Value) -> Value {
    let Some(fields) = input.as_object() else {
        return input.clone();
    };
    let mut out = serde_json::Map::with_capacity(fields.len());
    for (key, value) in fields {
        let capped = value.as_str().and_then(capped_view);
        out.insert(
            key.clone(),
            capped.map_or_else(|| value.clone(), Value::String),
        );
    }
    Value::Object(out)
}

/// Undo [`cap_tool_input`] for every field the hook handed back byte-identical
/// to the capped form, so capping can never truncate what actually executes. A
/// field the hook genuinely rewrote does not match the capped form, so it is
/// left exactly as the hook wrote it.
/// INVARIANT: the executed input differs from the model's only where a hook
/// deliberately rewrote it. Enforced by
/// `capping_a_tool_input_is_reversible_when_the_hook_echoes_it_back`,
/// `a_deliberate_rewrite_of_a_capped_field_still_wins`, and
/// `the_hook_sees_a_capped_input_but_the_tool_gets_the_full_one`.
pub fn restore_capped(original: &Value, mut rewritten: Value) -> Value {
    if let (Some(fields), Some(out)) = (original.as_object(), rewritten.as_object_mut()) {
        for (key, value) in fields {
            let Some(capped) = value.as_str().and_then(capped_view) else {
                continue;
            };
            if out.get(key).and_then(Value::as_str) == Some(capped.as_str()) {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    rewritten
}

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

/// A session-scoped registry of in-flight detached `notify` task handles
/// (Finding 1 fix, the Critical review caught in the real CLI): `notify`
/// pushes the `JoinHandle` it spawns here instead of discarding it, so a
/// caller that's about to tear down its runtime — the one-shot CLI's
/// `current_thread` `block_on`, which drops the instant its driving future
/// resolves — can `drain` them first, under a short bounded grace period.
/// Without this, a detached `tokio::spawn` awaiting a real subprocess (a
/// `notification` shell hook) is silently killed mid-flight the moment
/// `block_on` returns: nothing else is left polling it, unlike the
/// long-lived TUI/interactive path (or a `#[tokio::test]` that keeps polling
/// `events.recv()` in a loop) where the runtime stays alive on its own.
/// Cloning shares the same underlying registry (an `Arc`), so `notify`'s
/// caller (the actor/turn) and the CLI's exit-time drain call see the same
/// set of handles.
#[derive(Clone, Default)]
pub struct NotificationDrain(Arc<Mutex<Vec<JoinHandle<()>>>>);

impl NotificationDrain {
    pub fn new() -> Self {
        Self::default()
    }

    fn track(&self, handle: JoinHandle<()>) {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // Bound the registry's steady-state size across a long session:
        // drop handles that already finished rather than let it grow
        // unbounded (a poisoned lock has no invariants worth protecting —
        // recover and keep going, same stance `SharedDeps`/`current_turn`
        // take elsewhere).
        guard.retain(|h| !h.is_finished());
        guard.push(handle);
    }

    /// Await every still-pending handle, bounded by `grace` — never longer.
    /// A handle that hasn't finished when `grace` elapses is abandoned
    /// (best-effort, exactly like the per-hook timeout itself): this never
    /// blocks the caller past `grace`, so a hung notifier can't wedge the
    /// process's exit either.
    pub async fn drain(&self, grace: Duration) {
        let handles: Vec<_> = {
            let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut *guard)
        };
        if handles.is_empty() {
            return;
        }
        let _ = tokio::time::timeout(grace, futures_util::future::join_all(handles)).await;
    }
}

/// `Notification` (tier-1 gap #7, the `hotl watch`/desktop seam): spawn
/// `on_notification` **detached**, under its own timeout, and return
/// immediately — the caller MUST NOT `.await` this. A 2s (or hung) notifier
/// must never stall the turn or the actor loop that calls it (Concurrency &
/// parallelism §"Background (fire-and-forget) hooks"). The spawned handle is
/// tracked in `drain` (Finding 1) so a caller about to tear down its runtime
/// can wait for it first instead of silently killing it mid-flight.
pub fn notify(
    hooks: &Arc<dyn Hooks>,
    drain: &NotificationDrain,
    kind: NotificationKind,
    detail: impl Into<String>,
) {
    let hooks = Arc::clone(hooks);
    let detail = detail.into();
    let handle = tokio::spawn(async move {
        let _ =
            tokio::time::timeout(NOTIFICATION_TIMEOUT, hooks.on_notification(kind, &detail)).await;
    });
    drain.track(handle);
}

/// Await [`Hooks::on_stop`] under [`HOOK_CALL_TIMEOUT`]; a timeout behaves
/// exactly like `Allow` — a hung hook can never wedge a turn (it's a no-op,
/// not a block).
pub async fn call_stop(hooks: &Arc<dyn Hooks>, outcome: &str) -> StopDecision {
    tokio::time::timeout(HOOK_CALL_TIMEOUT, hooks.on_stop(outcome))
        .await
        .unwrap_or(StopDecision::Allow)
}

/// `SessionEnd`: run AWAITED, under its own bounded timeout, at actor
/// shutdown — NOT detached (Finding 1 fix). By definition nothing needs the
/// actor responsive once it's already shutting down for good, so blocking
/// here is correct, and it's the only way to *guarantee* the hook fires: a
/// detached task here would race the same runtime-drop that silently killed
/// `notify`'s old detached shape in the one-shot CLI path. The caller
/// (`actor::run`) is itself a spawned task the CLI now awaits to completion
/// before returning (`SessionHandle::finish`), so awaiting here — rather
/// than spawning — is what makes that wait actually cover the hook call.
pub async fn call_session_end(hooks: &Arc<dyn Hooks>) {
    let _ = tokio::time::timeout(NOTIFICATION_TIMEOUT, hooks.on_session_end()).await;
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
/// `Continue`. `results` is in **registration order** — the caller collects
/// matching hooks in a plain loop (T3-10 removed the `join_all` that implied a
/// concurrency these synchronous handlers never had), so a tie among
/// same-severity decisions always resolves to the first-registered hook, never
/// a race. Exposed (not just used internally by `InProcessHooks`) so lane 2
/// (the shell adapter) shares the exact same merge discipline instead of
/// re-implementing it.
/// INVARIANT: ties resolve by registration order, never by completion order.
/// Enforced by `ties_among_same_severity_decisions_resolve_by_registration_order`
/// and `in_process_hooks_run_in_registration_order`.
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
        // These handlers are synchronous `Fn`s: `join_all` over futures that are
        // ready on first poll bought concurrency-shaped syntax and no
        // concurrency (T3-10). Registration order is the tiebreak
        // `merge_pre_tool` documents, and a loop states that plainly.
        // INVARIANT: an in-process hook must not block — it runs inline on the
        // caller's task, bounded only by `call_pre_tool`'s timeout.
        Box::pin(async move {
            let mut results = Vec::new();
            for (matcher, hook) in &self.pre {
                if matcher.matches(name) {
                    results.push(hook(name, input));
                }
            }
            merge_pre_tool(results)
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
            let results: Vec<_> = self.prompt.iter().filter_map(|hook| hook(prompt)).collect();
            join_additional_context(results.into_iter())
        })
    }
    fn on_notification<'a>(&'a self, kind: NotificationKind, detail: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.notification {
                hook(kind, detail);
            }
        })
    }
    fn on_stop<'a>(&'a self, outcome: &'a str) -> BoxFuture<'a, StopDecision> {
        Box::pin(async move {
            let results: Vec<_> = self.stop.iter().map(|hook| hook(outcome)).collect();
            merge_stop(results)
        })
    }
    fn on_session_end<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.session_end {
                hook();
            }
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
    use tokio_util::sync::CancellationToken;

    struct HangingHooks;

    impl Hooks for HangingHooks {
        fn pre_tool<'a>(&'a self, _n: &'a str, _i: &'a Value) -> BoxFuture<'a, PreToolDecision> {
            Box::pin(std::future::pending())
        }
        fn post_tool<'a>(&'a self, _n: &'a str, _r: &'a str) -> BoxFuture<'a, Option<String>> {
            Box::pin(std::future::pending())
        }
    }

    /// T2-12. The paused clock is legal here: these drive the call wrappers
    /// alone, with no actor and therefore no writer-thread ack for the clock to
    /// auto-advance past (0011's standing constraint).
    #[tokio::test(start_paused = true)]
    async fn a_hung_pre_tool_hook_times_out_instead_of_wedging_the_turn() {
        let hooks: Arc<dyn Hooks> = Arc::new(HangingHooks);
        let cancel = CancellationToken::new();
        assert_eq!(
            call_pre_tool(&hooks, "bash", &json!({}), &cancel).await,
            PreToolDecision::Continue,
            "a hung pre_tool degrades to a no-op — never a grant, never a block"
        );
        assert_eq!(
            call_post_tool(&hooks, "bash", "the real output", &cancel).await,
            None,
            "a hung post_tool leaves the tool's real output in place"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_turn_does_not_wait_out_a_hook_timeout() {
        let hooks: Arc<dyn Hooks> = Arc::new(HangingHooks);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let start = tokio::time::Instant::now();
        assert_eq!(
            call_pre_tool(&hooks, "bash", &json!({}), &cancel).await,
            PreToolDecision::Continue
        );
        assert_eq!(call_post_tool(&hooks, "bash", "out", &cancel).await, None);
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "an interrupt must not wait out HOOK_CALL_TIMEOUT"
        );
    }

    /// T2-12b, the subtle half: capping is a *view*. A hook that hands the view
    /// straight back must not silently truncate what actually executes.
    #[test]
    fn capping_a_tool_input_is_reversible_when_the_hook_echoes_it_back() {
        let body = "z".repeat(HOOK_PAYLOAD_CAP * 2);
        let input = json!({"path": "notes.txt", "content": body});
        let view = cap_tool_input(&input);
        let seen = view["content"].as_str().expect("string field");
        assert!(
            seen.len() <= HOOK_PAYLOAD_CAP + CAP_MARK.len(),
            "the hook saw {} bytes",
            seen.len()
        );
        assert_eq!(
            view["path"],
            json!("notes.txt"),
            "fields under the cap pass through untouched"
        );
        let restored = restore_capped(&input, view);
        assert_eq!(restored, input, "capping must be observationally inert");
    }

    #[test]
    fn a_deliberate_rewrite_of_a_capped_field_still_wins() {
        let body = "z".repeat(HOOK_PAYLOAD_CAP * 2);
        let input = json!({"path": "notes.txt", "content": body});
        let mut view = cap_tool_input(&input);
        view["content"] = json!("redacted by policy");
        let restored = restore_capped(&input, view);
        assert_eq!(
            restored["content"],
            json!("redacted by policy"),
            "restoration must never undo a hook's real rewrite"
        );
        assert_eq!(restored["path"], json!("notes.txt"));
    }

    #[test]
    fn capping_leaves_non_object_inputs_and_nested_values_alone() {
        let nested = json!({"outer": {"inner": "z".repeat(HOOK_PAYLOAD_CAP * 2)}});
        assert_eq!(
            cap_tool_input(&nested),
            nested,
            "nested values are not capped"
        );
        let scalar = json!("just a string");
        assert_eq!(cap_tool_input(&scalar), scalar);
        assert_eq!(
            restore_capped(&scalar, json!("rewritten")),
            json!("rewritten")
        );
    }

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

    /// T3-10's merge criterion. In-process handlers are synchronous `Fn`s, so
    /// this pinned current observable behavior *before* `join_all` was replaced
    /// with a plain loop, and must keep passing after: registration order is
    /// the tiebreak `merge_stop`/`merge_pre_tool` document, and it is now
    /// stated plainly rather than implied by `join_all`'s input ordering.
    #[tokio::test]
    async fn in_process_hooks_run_in_registration_order() {
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let push = |order: &Arc<Mutex<Vec<&'static str>>>, tag: &'static str| {
            let order = Arc::clone(order);
            move |_outcome: &str| {
                order.lock().expect("lock").push(tag);
                StopDecision::Allow
            }
        };
        let hooks = InProcessHooks::new()
            .on_stop(push(&order, "a"))
            .on_stop(push(&order, "b"))
            .on_stop(push(&order, "c"));
        // Disambiguated: the builder's `on_stop` shadows the trait's.
        assert_eq!(Hooks::on_stop(&hooks, "done").await, StopDecision::Allow);
        assert_eq!(*order.lock().expect("lock"), vec!["a", "b", "c"]);
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
