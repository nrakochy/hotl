//! L4 — the tool system.
//!
//! Four built-ins (read / edit / write / bash), a permission-gate seam, and
//! the protected execute-later path class (SECURITY.md). Every failure
//! message is a prompt: it tells the model what to do next. Erasure happens
//! once: tools are `dyn Tool` in the registry.

mod builtins;
pub mod diagnostics;
pub(crate) mod matcher;
pub mod net;
pub mod rules;
pub mod sandbox;
pub mod skills;
pub mod todo;

pub use builtins::{BashTool, EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
pub use todo::TodoWriteTool;

use std::sync::Arc;

use futures_util::future::BoxFuture;
use hotl_provider::ToolDef;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// What executing a call requires from the human on the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    /// Read-only: runs without asking.
    None,
    /// Mutating/executing: y/n ask with this one-line summary.
    Ask { summary: String },
    /// Write into the execute-later class: escalated warning ask.
    AskProtected { summary: String, why: String },
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    /// Errors-as-prompts: `content` must tell the model how to proceed.
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    fn permission(&self, input: &Value) -> Permission;
    /// May calls to this tool run concurrently with other parallel-safe calls
    /// in the same assistant batch? Only for tools whose calls cannot observe
    /// or affect each other (pure reads, isolated child sessions). Mutating or
    /// executing tools stay serial: the default is the safe answer.
    fn parallel_safe(&self) -> bool {
        false
    }
    /// Does this tool only ever read (no filesystem mutation, no execution,
    /// no network side effects)? Read-only tools run in plan mode; everything
    /// else is blocked there. Like `parallel_safe`, the default is the safe
    /// answer (false), and each read tool opts in.
    fn read_only(&self) -> bool {
        false
    }
    fn run<'a>(&'a self, input: Value, cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome>;
}

/// The human on the loop. Native CLI implements a y/n prompt; headless
/// default-denies. No allow-rule persistence exists until the
/// M1 sandbox floor — every ask is asked.
pub trait PermissionGate: Send + Sync {
    fn ask<'a>(&'a self, summary: &'a str, protected_why: Option<&'a str>) -> BoxFuture<'a, bool>;
}

/// Test/headless gates.
pub struct StaticGate(pub bool);
impl PermissionGate for StaticGate {
    fn ask<'a>(&'a self, _s: &'a str, _p: Option<&'a str>) -> BoxFuture<'a, bool> {
        Box::pin(std::future::ready(self.0))
    }
}

/// Tools are `Arc<dyn Tool>`, not `Box<dyn Tool>`: a `Registry` is cheap to
/// `Clone` (an Arc-bump per tool, no re-walk of skill roots or MCP config),
/// which lets a caller layer one session-specific tool (`todo_write`, wired
/// to that session's own actor) onto the process-wide shared registry
/// without rebuilding it or making every tool instance `Clone` itself.
#[derive(Clone)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn builtin() -> Self {
        Self::builtin_with(diagnostics::Diagnostics::default())
    }

    /// Builtins with post-mutation diagnostics (M3a) shared by edit/write.
    pub fn builtin_with(diag: diagnostics::Diagnostics) -> Self {
        let diag = Arc::new(diag);
        Self {
            tools: vec![
                Arc::new(ReadTool),
                Arc::new(EditTool { diag: diag.clone() }),
                Arc::new(WriteTool { diag }),
                Arc::new(BashTool),
                Arc::new(GlobTool),
                Arc::new(GrepTool),
            ],
        }
    }

    /// Register an additional tool (MCP meta-tool, skills — M3).
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(Arc::from(tool));
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.schema(),
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|b| b.as_ref())
    }
}

/// The execute-later class: files whose *write* is benign-looking but
/// whose later effect —
/// execution, authentication, or credential theft — happens outside any gate.
/// Writes here get the escalated warning ask instead of an ordinary one.
pub fn execute_later_reason(path: &str) -> Option<&'static str> {
    let p = path.trim_start_matches("./");
    let file = p.rsplit('/').next().unwrap_or(p);
    if p.contains(".git/hooks/") {
        return Some("git hook: runs on your next git command");
    }
    if matches!(
        file,
        "Makefile" | "makefile" | "GNUmakefile" | "justfile" | "Justfile"
    ) {
        return Some("build entrypoint: runs on your next make/just invocation");
    }
    // Build-time code entrypoints: a diagnostic or a plain `cargo build` /
    // `pytest` compiles and runs these (H-11's write-now/execute-later path).
    if matches!(file, "build.rs" | "conftest.py") || file.ends_with(".gyp") {
        return Some("build-time code: compiled and run by your build/test tooling");
    }
    if matches!(file, "AGENTS.md" | "CLAUDE.md") {
        return Some("agent instructions: injected into future model contexts");
    }
    if file == "settings.json" || p.contains(".hotl/") || p.contains(".claude/") {
        return Some("harness settings/hooks: change how future sessions behave");
    }
    // hotl's own config: allow rules live here, and [provider] api_key_helper
    // runs an arbitrary command at next startup, outside the tool sandbox.
    if p.contains(".config/hotl/") {
        return Some("hotl config: allow rules and the api-key-helper command run from here");
    }
    if file.ends_with(".zshrc") || file.ends_with(".bashrc") || file.ends_with(".profile") {
        return Some("shell startup file: runs in every new shell");
    }
    // SSH: authorized_keys grants login; config can rewrite where ssh connects.
    if p.contains(".ssh/") {
        return Some("SSH config/keys: can grant login or redirect your connections");
    }
    // Cloud + package-registry credentials: write-to-steal or token planting.
    if p.contains(".aws/")
        || p.contains(".config/gcloud/")
        || p.contains(".azure/")
        || matches!(file, ".npmrc" | ".pypirc" | ".netrc" | ".dockercfg")
    {
        return Some("credentials file: writing here can steal or plant auth tokens");
    }
    // git config aliases / hooksPath run as commands on the next git call.
    if file == ".gitconfig" || p.contains(".git/config") {
        return Some("git config: aliases here run as commands on your next git call");
    }
    // Schedulers and service definitions run code on a timer / at boot.
    if p.contains("/cron.")
        || file == "crontab"
        || p.contains("/systemd/")
        || file.ends_with(".service")
    {
        return Some("scheduler/service unit: runs code on a timer or at boot");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_includes_search_tools() {
        let reg = Registry::builtin();
        for name in ["read", "edit", "write", "bash", "glob", "grep"] {
            assert!(reg.get(name).is_some(), "missing built-in `{name}`");
        }
        // Search tools are read-only and parallel-safe.
        assert!(reg.get("glob").unwrap().parallel_safe());
        assert!(reg.get("grep").unwrap().parallel_safe());
        assert_eq!(
            reg.get("glob").unwrap().permission(&serde_json::json!({})),
            Permission::None
        );
    }

    #[test]
    fn protected_paths() {
        assert!(execute_later_reason(".git/hooks/pre-commit").is_some());
        assert!(execute_later_reason("sub/dir/Makefile").is_some());
        assert!(execute_later_reason("AGENTS.md").is_some());
        assert!(execute_later_reason(".hotl/settings.json").is_some());
        assert!(execute_later_reason("src/main.rs").is_none());
        assert!(execute_later_reason("docs/notes.md").is_none());
    }

    #[test]
    fn read_only_is_true_only_for_pure_reads() {
        let reg = Registry::builtin();
        assert!(reg.get("read").unwrap().read_only());
        assert!(reg.get("glob").unwrap().read_only());
        assert!(reg.get("grep").unwrap().read_only());
        assert!(!reg.get("write").unwrap().read_only());
        assert!(!reg.get("edit").unwrap().read_only());
        assert!(!reg.get("bash").unwrap().read_only()); // bash can mutate; not read-only
    }

    #[test]
    fn protected_paths_cover_creds_and_build_entrypoints() {
        // The H-04 additions: credential, scheduler, and build-code targets.
        for p in [
            "home/user/.ssh/authorized_keys",
            "home/user/.ssh/config",
            "home/user/.aws/credentials",
            "home/user/.config/gcloud/creds",
            "project/.npmrc",
            "home/user/.pypirc",
            "home/user/.netrc",
            "home/user/.gitconfig",
            "repo/.git/config",
            "etc/cron.d/job",
            "etc/systemd/system/x.service",
            "crate/build.rs",
            "tests/conftest.py",
            "/Users/x/.config/hotl/config.toml",
        ] {
            assert!(execute_later_reason(p).is_some(), "{p} should be protected");
        }
        // Ordinary source and docs stay unescalated.
        assert!(execute_later_reason("src/lib.rs").is_none());
        assert!(execute_later_reason("README.md").is_none());
    }
}
