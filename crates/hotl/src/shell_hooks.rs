//! Lane 2 — the Claude-compatible shell-hook adapter (M5, tier-1 gap #7).
//!
//! Owner-configured commands in config.toml's `[[hook]]` run at the six
//! events lane 1 exposes: `pre_tool`/`post_tool` (matcher-filtered), plus
//! `user_prompt`/`notification`/`stop`/`session_end`. A hook command receives
//! the event as JSON on stdin and returns a decision as JSON on stdout, runs
//! **under the sandbox floor** (it is a command, not trusted-by-position),
//! sees byte-capped payloads, draws a `SessionConcurrency::subproc()` permit
//! per process (the same fork-storm guard `bash`/`grep` share), and is
//! **evicted after 3 failures in a session** (RELIABILITY.md repeat-offender
//! rule). A malformed or failed decision is a no-op — a shell hook can
//! *block* or *add context* but can never *grant* (fail-open on decision,
//! never on permission).
//!
//! ```toml
//! # ~/.config/hotl/config.toml
//! [[hook]]
//! event = "pre_tool"          # pre_tool | post_tool | user_prompt | notification | stop | session_end
//! command = "/usr/local/bin/guard"
//! matcher = "bash,write"      # exact tool names, comma-separated; "*"/absent = all tools (pre_tool/post_tool only)
//! env = { FOO = "bar" }       # optional extra env for the command — never overrides identity env (below)
//! ```
//!
//! Wire protocol (stdin → the hook), unchanged for the original two events:
//!   {"event":"pre_tool","tool":"bash","input":{...}}
//!   {"event":"post_tool","tool":"read","result":"<capped>"}
//! and the new events:
//!   {"event":"user_prompt","prompt":"..."}
//!   {"event":"notification","kind":"blocked"|"idle"|"done","detail":"..."}
//!   {"event":"stop","outcome":"..."}
//!   {"event":"session_end"}
//! Decision (hook stdout → us):
//!   pre_tool:      {"decision":"continue"}
//!                | {"decision":"deny","message":"why"}
//!                | {"decision":"rewrite","input":{...}}
//!   post_tool:     {"result":"replacement"}   (absent/empty ⇒ unchanged)
//!   user_prompt:   {"hookSpecificOutput":{"additionalContext":"..."}} — the
//!                  Claude schema shape verbatim (12 §Q3), so a
//!                  `~/.claude/settings.json`-style `additionalContext` hook
//!                  ports unmodified.
//!   notification:  (ignored — fire-and-forget)
//!   stop:          {"decision":"block","reason":"why"} | {"decision":"allow"} (default)
//!   session_end:   (ignored — fire-and-forget)
//!
//! Identity env (`HOTL_HOOK_EVENT`) is applied **after** the `[[hook]] env`
//! table, so a hook config cannot spoof the event its own script observes
//! (03 lesson 5).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::future::BoxFuture;
use hotl_engine::hooks::{
    cap_payload, join_additional_context, merge_pre_tool, merge_stop, Hooks, Matcher,
    NotificationKind, PreToolDecision, StopDecision,
};
use hotl_tools::concurrency::SessionConcurrency;
use hotl_tools::{net, sandbox};
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_STRIKES: u32 = 3;
const HOOK_TIMEOUT_SECS: u64 = 10;
/// A decision is tiny JSON; a hook that floods stdout past this is malformed.
const HOOK_MAX_OUTPUT: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
struct HookSpec {
    event: String,
    command: String,
    /// Exact tool names, comma-separated, or `"*"`/absent for every tool.
    /// Only meaningful for `pre_tool`/`post_tool`; ignored elsewhere.
    #[serde(default)]
    matcher: Option<String>,
    /// Extra env for the command — owner-supplied convenience, layered
    /// *before* identity env so it can never override `HOTL_HOOK_EVENT`.
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default, rename = "hook")]
    hooks: Vec<HookSpec>,
}

/// `"*"`, absent, or empty → every tool; otherwise an exact-name comma list.
fn parse_matcher(raw: Option<&str>) -> Matcher {
    match raw.map(str::trim) {
        None | Some("") | Some("*") => Matcher::All,
        Some(s) => Matcher::Names(
            s.split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect(),
        ),
    }
}

struct ShellHook {
    command: String,
    env: HashMap<String, String>,
    strikes: AtomicU32,
}

pub struct ShellHooks {
    pre: Vec<(Matcher, ShellHook)>,
    post: Vec<(Matcher, ShellHook)>,
    prompt: Vec<ShellHook>,
    notification: Vec<ShellHook>,
    stop: Vec<ShellHook>,
    session_end: Vec<ShellHook>,
    /// The one shared Layer-B budget (`SessionConcurrency`) — every shell
    /// hook process draws a `subproc()` permit here, the same pool
    /// `bash`/`grep` draw from, so a turn firing a dozen matching hooks plus
    /// a `grep` never exceeds the configured concurrent-process width.
    concurrency: SessionConcurrency,
}

/// Parse shell hooks from a TOML string (the `[[hook]]` section of
/// config.toml, fed in by the binary), threading in the process-wide
/// `SessionConcurrency` every hook process draws its permit from. `None` if
/// none are configured.
pub fn load_str(raw: &str, concurrency: SessionConcurrency) -> Option<ShellHooks> {
    let parsed: HooksFile = toml::from_str(raw).ok()?;
    let mut pre = Vec::new();
    let mut post = Vec::new();
    let mut prompt = Vec::new();
    let mut notification = Vec::new();
    let mut stop = Vec::new();
    let mut session_end = Vec::new();
    for spec in parsed.hooks {
        let matcher = parse_matcher(spec.matcher.as_deref());
        let hook = ShellHook {
            command: spec.command,
            env: spec.env,
            strikes: AtomicU32::new(0),
        };
        match spec.event.as_str() {
            "pre_tool" => pre.push((matcher, hook)),
            "post_tool" => post.push((matcher, hook)),
            "user_prompt" => prompt.push(hook),
            "notification" => notification.push(hook),
            "stop" => stop.push(hook),
            "session_end" => session_end.push(hook),
            _ => {} // unknown event: ignored (forward-compat)
        }
    }
    if pre.is_empty()
        && post.is_empty()
        && prompt.is_empty()
        && notification.is_empty()
        && stop.is_empty()
        && session_end.is_empty()
    {
        return None;
    }
    Some(ShellHooks {
        pre,
        post,
        prompt,
        notification,
        stop,
        session_end,
        concurrency,
    })
}

impl ShellHook {
    /// Run the command with `payload` on stdin; `None` if evicted, timed out,
    /// failed, or produced no parseable stdout. `event` becomes the
    /// `HOTL_HOOK_EVENT` identity env var, applied strictly after the
    /// hook's own `env` table so a `[[hook]] env` setting of the same key
    /// can never spoof it.
    async fn invoke(
        &self,
        payload: &Value,
        event: &str,
        concurrency: &SessionConcurrency,
    ) -> Option<Value> {
        if self.strikes.load(Ordering::Relaxed) >= MAX_STRIKES {
            return None; // evicted for the session
        }
        // The runaway-spawn guard: one permit per hook *process*, drawn from
        // the same shared pool bash/grep use — held for this call's whole
        // lifetime, released on drop when the function returns.
        let _permit = concurrency.subproc().await;
        let egress = net::egress_state().await;
        let mut cmd = sandbox::build_command(&self.command, &sandbox::probe(), &egress);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        // Identity env applied LAST: whatever the hook's own `env` table set
        // for this key is overwritten here — it cannot spoof the real event.
        cmd.env("HOTL_HOOK_EVENT", event);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let child = cmd.spawn().ok();
        let Some(mut child) = child else {
            self.strike();
            return None;
        };
        // Everything that can block on the child — the stdin write included —
        // runs inside the timeout: a hook that never drains stdin would
        // otherwise park write_all forever once the payload exceeds the OS
        // pipe buffer, wedging the turn with the clock never started. The
        // write and the read overlap so a hook that streams output before
        // finishing stdin can't deadlock the pair of pipes either.
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take();
        let payload = payload.to_string();
        let out = tokio::time::timeout(std::time::Duration::from_secs(HOOK_TIMEOUT_SECS), async {
            let write = async {
                if let Some(mut stdin) = stdin {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(payload.as_bytes()).await;
                    let _ = stdin.shutdown().await;
                } // dropped here — the hook sees stdin EOF
            };
            // Read stdout capped rather than wait_with_output: past the cap
            // the pipe closes, an over-chatty hook dies on SIGPIPE, and it
            // strikes.
            let read = async {
                use tokio::io::AsyncReadExt;
                let mut buf = Vec::new();
                if let Some(so) = stdout.as_mut() {
                    let _ = so
                        .take((HOOK_MAX_OUTPUT + 1) as u64)
                        .read_to_end(&mut buf)
                        .await;
                }
                buf
            };
            let ((), buf) = tokio::join!(write, read);
            drop(stdout);
            (child.wait().await, buf)
        })
        .await;
        match out {
            Ok((Ok(status), buf)) if status.success() && buf.len() <= HOOK_MAX_OUTPUT => {
                serde_json::from_slice(&buf).ok().or_else(|| {
                    // Exited 0 but no JSON: treat as continue (not a failure).
                    Some(json!({"decision": "continue"}))
                })
            }
            _ => {
                self.strike();
                None
            }
        }
    }

    fn strike(&self) {
        let n = self.strikes.fetch_add(1, Ordering::Relaxed) + 1;
        if n == MAX_STRIKES {
            eprintln!(
                "hotl: hook `{}` failed {MAX_STRIKES}× — evicted for this session",
                self.command
            );
        }
    }
}

impl Hooks for ShellHooks {
    fn pre_tool<'a>(&'a self, name: &'a str, input: &'a Value) -> BoxFuture<'a, PreToolDecision> {
        Box::pin(async move {
            let payload = json!({"event": "pre_tool", "tool": name, "input": input});
            // Every matching hook runs concurrently (they're subprocess-
            // latency-bound); results are collected and folded in
            // REGISTRATION order (never completion order — `join_all`
            // preserves input order), so a fast hook can't race a slow,
            // more-restrictive one.
            let futures = self
                .pre
                .iter()
                .filter(|(matcher, _)| matcher.matches(name))
                .map(|(_, hook)| {
                    let payload = payload.clone();
                    async move {
                        match hook.invoke(&payload, "pre_tool", &self.concurrency).await {
                            Some(decision) => decode_pre_tool(&decision),
                            None => PreToolDecision::Continue,
                        }
                    }
                });
            merge_pre_tool(futures_util::future::join_all(futures).await)
        })
    }

    fn post_tool<'a>(&'a self, name: &'a str, result: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let capped = cap_payload(result);
            let mut current: Option<String> = None;
            // Node-style proposal chain (each hook refines the previous
            // one's output) — inherently sequential, matching `InProcessHooks`.
            for (matcher, hook) in &self.post {
                if !matcher.matches(name) {
                    continue;
                }
                let view = current.as_deref().unwrap_or(capped);
                let payload = json!({"event": "post_tool", "tool": name, "result": view});
                if let Some(decision) = hook.invoke(&payload, "post_tool", &self.concurrency).await
                {
                    if let Some(replacement) = decision.get("result").and_then(Value::as_str) {
                        if !replacement.is_empty() {
                            current = Some(replacement.to_string());
                        }
                    }
                }
            }
            current
        })
    }

    fn on_user_prompt<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let payload = json!({"event": "user_prompt", "prompt": prompt});
            let futures = self.prompt.iter().map(|hook| {
                let payload = payload.clone();
                async move {
                    let decision = hook
                        .invoke(&payload, "user_prompt", &self.concurrency)
                        .await?;
                    decision
                        .get("hookSpecificOutput")
                        .and_then(|h| h.get("additionalContext"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }
            });
            let results = futures_util::future::join_all(futures).await;
            join_additional_context(results.into_iter().flatten())
        })
    }

    fn on_notification<'a>(&'a self, kind: NotificationKind, detail: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let kind_str = match kind {
                NotificationKind::Blocked => "blocked",
                NotificationKind::Idle => "idle",
                NotificationKind::Done => "done",
            };
            let payload = json!({"event": "notification", "kind": kind_str, "detail": detail});
            // The caller (`hotl_engine::hooks::notify`) already spawned this
            // whole call detached with its own timeout — awaiting every
            // shell hook here is safe; it never touches the turn's hot path.
            // Each process still draws its own `subproc()` permit, so a
            // burst of notifications can't fork-storm.
            let futures = self
                .notification
                .iter()
                .map(|hook| hook.invoke(&payload, "notification", &self.concurrency));
            futures_util::future::join_all(futures).await;
        })
    }

    fn on_stop<'a>(&'a self, outcome: &'a str) -> BoxFuture<'a, StopDecision> {
        Box::pin(async move {
            let payload = json!({"event": "stop", "outcome": outcome});
            let futures = self.stop.iter().map(|hook| {
                let payload = payload.clone();
                async move {
                    match hook.invoke(&payload, "stop", &self.concurrency).await {
                        Some(decision) => decode_stop(&decision),
                        None => StopDecision::Allow,
                    }
                }
            });
            merge_stop(futures_util::future::join_all(futures).await)
        })
    }

    fn on_session_end<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let payload = json!({"event": "session_end"});
            let futures = self
                .session_end
                .iter()
                .map(|hook| hook.invoke(&payload, "session_end", &self.concurrency));
            futures_util::future::join_all(futures).await;
        })
    }
}

fn decode_pre_tool(decision: &Value) -> PreToolDecision {
    match decision.get("decision").and_then(Value::as_str) {
        Some("deny") => {
            let message = decision
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("blocked by a hook")
                .to_string();
            PreToolDecision::Deny { message }
        }
        Some("rewrite") => match decision.get("input") {
            Some(input) => PreToolDecision::Rewrite {
                input: input.clone(),
            },
            None => PreToolDecision::Continue,
        },
        _ => PreToolDecision::Continue, // "continue"/unknown → no opinion
    }
}

fn decode_stop(decision: &Value) -> StopDecision {
    match decision.get("decision").and_then(Value::as_str) {
        Some("block") => {
            let reason = decision
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("blocked by a hook")
                .to_string();
            StopDecision::Block { reason }
        }
        _ => StopDecision::Allow, // "allow"/unknown/absent → no veto
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concurrency() -> SessionConcurrency {
        SessionConcurrency::new(hotl_tools::concurrency::ConcurrencyLimits::default())
    }

    #[tokio::test]
    async fn pre_hook_denies_over_stdio() {
        // A hook that reads the event on stdin and denies bash calls.
        let hooks = load_str(
            "[[hook]]\nevent = \"pre_tool\"\n\
             command = \"cat >/dev/null; echo '{\\\"decision\\\":\\\"deny\\\",\\\"message\\\":\\\"shell says no\\\"}'\"\n",
            concurrency(),
        ).expect("hooks configured");
        let decision = hooks.pre_tool("bash", &json!({"command": "ls"})).await;
        assert_eq!(
            decision,
            PreToolDecision::Deny {
                message: "shell says no".into()
            }
        );
    }

    #[tokio::test]
    async fn post_hook_replaces_result_and_none_when_unconfigured() {
        let hooks = load_str(
            "[[hook]]\nevent = \"post_tool\"\n\
             command = \"cat >/dev/null; echo '{\\\"result\\\":\\\"cleaned\\\"}'\"\n",
            concurrency(),
        )
        .unwrap();
        assert_eq!(
            hooks.post_tool("read", "raw output").await.as_deref(),
            Some("cleaned")
        );
        // A config with no hooks loads as None.
        assert!(load_str("# no hooks here\n", concurrency()).is_none());
    }

    #[tokio::test]
    async fn failing_hook_is_evicted_after_three_strikes() {
        let hooks = load_str(
            "[[hook]]\nevent = \"pre_tool\"\ncommand = \"exit 1\"\n",
            concurrency(),
        )
        .unwrap();
        // A failing hook is a no-op (continue), and after 3 strikes it's evicted
        // (still continue — a hook can block but never grant).
        for _ in 0..5 {
            assert_eq!(
                hooks.pre_tool("bash", &json!({})).await,
                PreToolDecision::Continue
            );
        }
        assert!(hooks.pre[0].1.strikes.load(Ordering::Relaxed) >= MAX_STRIKES);
    }

    #[tokio::test]
    async fn matcher_scopes_a_shell_hook_to_named_tools() {
        let hooks = load_str(
            "[[hook]]\nevent = \"pre_tool\"\nmatcher = \"bash\"\n\
             command = \"cat >/dev/null; echo '{\\\"decision\\\":\\\"deny\\\",\\\"message\\\":\\\"no\\\"}'\"\n",
            concurrency(),
        )
        .unwrap();
        assert_eq!(
            hooks.pre_tool("bash", &json!({})).await,
            PreToolDecision::Deny {
                message: "no".into()
            }
        );
        // `read` doesn't match the `bash`-only matcher — no-op.
        assert_eq!(
            hooks.pre_tool("read", &json!({})).await,
            PreToolDecision::Continue
        );
    }

    #[tokio::test]
    async fn user_prompt_hook_returns_additional_context_via_the_claude_schema_shape() {
        let hooks = load_str(
            "[[hook]]\nevent = \"user_prompt\"\n\
             command = \"cat >/dev/null; echo '{\\\"hookSpecificOutput\\\":{\\\"additionalContext\\\":\\\"X\\\"}}'\"\n",
            concurrency(),
        )
        .unwrap();
        assert_eq!(hooks.on_user_prompt("hello").await.as_deref(), Some("X"));
    }

    #[tokio::test]
    async fn stop_hook_can_block_with_a_reason() {
        let hooks = load_str(
            "[[hook]]\nevent = \"stop\"\n\
             command = \"cat >/dev/null; echo '{\\\"decision\\\":\\\"block\\\",\\\"reason\\\":\\\"not yet\\\"}'\"\n",
            concurrency(),
        )
        .unwrap();
        assert_eq!(
            hooks.on_stop("done").await,
            StopDecision::Block {
                reason: "not yet".into()
            }
        );
    }

    #[tokio::test]
    async fn identity_env_is_not_spoofable_by_a_hooks_own_env_table() {
        // The hook's own `env` table tries to set HOTL_HOOK_EVENT to a lie;
        // the real event ("stop") must still win because identity env is
        // applied strictly after the hook's own env.
        let toml = r#"
[[hook]]
event = "stop"
command = "printf '{\"decision\":\"block\",\"reason\":\"%s\"}' \"$HOTL_HOOK_EVENT\""
env = { HOTL_HOOK_EVENT = "spoofed-should-not-win" }
"#;
        let hooks = load_str(toml, concurrency()).unwrap();
        let decision = hooks.on_stop("done").await;
        assert_eq!(
            decision,
            StopDecision::Block {
                reason: "stop".into()
            },
            "the hook must see the real event, not the one its own env tried to set"
        );
    }

    #[tokio::test]
    async fn notification_and_session_end_hooks_run_without_error() {
        let hooks = load_str(
            "[[hook]]\nevent = \"notification\"\ncommand = \"cat >/dev/null\"\n\
             [[hook]]\nevent = \"session_end\"\ncommand = \"cat >/dev/null\"\n",
            concurrency(),
        )
        .unwrap();
        hooks
            .on_notification(NotificationKind::Blocked, "waiting")
            .await;
        hooks.on_session_end().await;
    }
}
