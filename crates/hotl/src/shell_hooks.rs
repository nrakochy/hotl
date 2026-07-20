//! Lane 2 — the Claude-compatible shell-hook adapter (M5; 0007 §5).
//!
//! Owner-configured commands in `hooks.toml` run at the same two events lane 1
//! exposes. A hook command receives the event as JSON on stdin and returns a
//! decision as JSON on stdout, runs **under the sandbox floor** (it is a
//! command, not trusted-by-position), sees byte-capped payloads (0006 M5 pin
//! #1), and is **evicted after 3 failures in a session** (RELIABILITY.md
//! repeat-offender rule). A malformed or failed decision is a no-op — a shell
//! hook can *block* but can never *grant* (fail-open on decision, never on
//! permission).
//!
//! ```toml
//! # ~/.config/hotl/hooks.toml
//! [[hook]]
//! event = "pre_tool"          # or "post_tool"
//! command = "/usr/local/bin/guard"
//! ```
//!
//! Wire protocol (stdin → the hook):
//!   {"event":"pre_tool","tool":"bash","input":{...}}
//!   {"event":"post_tool","tool":"read","result":"<capped>"}
//! Decision (hook stdout → us):
//!   pre_tool:  {"decision":"continue"}
//!            | {"decision":"deny","message":"why"}
//!            | {"decision":"rewrite","input":{...}}
//!   post_tool: {"result":"replacement"}   (absent/empty ⇒ unchanged)

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::future::BoxFuture;
use hotl_engine::hooks::{cap_payload, Hooks, PreToolDecision};
use hotl_tools::sandbox;
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_STRIKES: u32 = 3;
const HOOK_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Deserialize)]
struct HookSpec {
    event: String,
    command: String,
}

#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default, rename = "hook")]
    hooks: Vec<HookSpec>,
}

struct ShellHook {
    command: String,
    strikes: AtomicU32,
}

pub struct ShellHooks {
    pre: Vec<ShellHook>,
    post: Vec<ShellHook>,
}

/// Load configured shell hooks, or `None` if none are configured.
pub fn load(config_dir: &Path) -> Option<ShellHooks> {
    let raw = std::fs::read_to_string(config_dir.join("hooks.toml")).ok()?;
    let parsed: HooksFile = toml::from_str(&raw).ok()?;
    let mut pre = Vec::new();
    let mut post = Vec::new();
    for spec in parsed.hooks {
        let hook = ShellHook { command: spec.command, strikes: AtomicU32::new(0) };
        match spec.event.as_str() {
            "pre_tool" => pre.push(hook),
            "post_tool" => post.push(hook),
            _ => {} // unknown event: ignored (forward-compat)
        }
    }
    if pre.is_empty() && post.is_empty() {
        return None;
    }
    Some(ShellHooks { pre, post })
}

impl ShellHook {
    /// Run the command with `payload` on stdin; `None` if evicted, timed out,
    /// failed, or produced no parseable stdout.
    async fn invoke(&self, payload: &Value) -> Option<Value> {
        if self.strikes.load(Ordering::Relaxed) >= MAX_STRIKES {
            return None; // evicted for the session
        }
        let mut cmd = sandbox::build_command(&self.command, &sandbox::probe());
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let child = cmd.spawn().ok();
        let Some(mut child) = child else {
            self.strike();
            return None;
        };
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(HOOK_TIMEOUT_SECS),
            child.wait_with_output(),
        )
        .await;
        match out {
            Ok(Ok(o)) if o.status.success() => {
                serde_json::from_slice(&o.stdout).ok().or_else(|| {
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
            eprintln!("hotl: hook `{}` failed {MAX_STRIKES}× — evicted for this session", self.command);
        }
    }
}

impl Hooks for ShellHooks {
    fn pre_tool<'a>(&'a self, name: &'a str, input: &'a Value) -> BoxFuture<'a, PreToolDecision> {
        Box::pin(async move {
            let payload = json!({"event": "pre_tool", "tool": name, "input": input});
            for hook in &self.pre {
                let Some(decision) = hook.invoke(&payload).await else { continue };
                match decision.get("decision").and_then(Value::as_str) {
                    Some("deny") => {
                        let message = decision
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("blocked by a hook")
                            .to_string();
                        return PreToolDecision::Deny { message };
                    }
                    Some("rewrite") => {
                        if let Some(new_input) = decision.get("input") {
                            return PreToolDecision::Rewrite { input: new_input.clone() };
                        }
                    }
                    _ => {} // "continue"/unknown → next hook
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
                let payload = json!({"event": "post_tool", "tool": name, "result": view});
                if let Some(decision) = hook.invoke(&payload).await {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(hooks_toml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hooks.toml"), hooks_toml).unwrap();
        dir
    }

    #[tokio::test]
    async fn pre_hook_denies_over_stdio() {
        // A hook that reads the event on stdin and denies bash calls.
        let dir = config_with(
            "[[hook]]\nevent = \"pre_tool\"\n\
             command = \"cat >/dev/null; echo '{\\\"decision\\\":\\\"deny\\\",\\\"message\\\":\\\"shell says no\\\"}'\"\n",
        );
        let hooks = load(dir.path()).expect("hooks configured");
        let decision = hooks.pre_tool("bash", &json!({"command": "ls"})).await;
        assert_eq!(decision, PreToolDecision::Deny { message: "shell says no".into() });
    }

    #[tokio::test]
    async fn post_hook_replaces_result_and_none_when_unconfigured() {
        let dir = config_with(
            "[[hook]]\nevent = \"post_tool\"\n\
             command = \"cat >/dev/null; echo '{\\\"result\\\":\\\"cleaned\\\"}'\"\n",
        );
        let hooks = load(dir.path()).unwrap();
        assert_eq!(hooks.post_tool("read", "raw output").await.as_deref(), Some("cleaned"));
        // A config with no hooks loads as None.
        let empty = config_with("# no hooks here\n");
        assert!(load(empty.path()).is_none());
    }

    #[tokio::test]
    async fn failing_hook_is_evicted_after_three_strikes() {
        let dir = config_with(
            "[[hook]]\nevent = \"pre_tool\"\ncommand = \"exit 1\"\n",
        );
        let hooks = load(dir.path()).unwrap();
        // A failing hook is a no-op (continue), and after 3 strikes it's evicted
        // (still continue — a hook can block but never grant).
        for _ in 0..5 {
            assert_eq!(
                hooks.pre_tool("bash", &json!({})).await,
                PreToolDecision::Continue
            );
        }
        assert!(hooks.pre[0].strikes.load(Ordering::Relaxed) >= MAX_STRIKES);
    }
}
