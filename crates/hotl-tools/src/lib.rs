//! L4 — the tool system (system-design §L4), M0 slice.
//!
//! Four built-ins (read / edit / write / bash), a permission-gate seam, and
//! the protected execute-later path class (SECURITY.md, r2 R3). Every failure
//! message is a prompt: it tells the model what to do next (core belief,
//! corpus 14). Erasure happens once: tools are `dyn Tool` in the registry.

mod builtins;
pub mod diagnostics;
pub mod skills;
pub(crate) mod matcher;
pub mod rules;
pub mod sandbox;

pub use builtins::{BashTool, EditTool, ReadTool, WriteTool};

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
        Self { content: content.into(), is_error: false }
    }
    /// Errors-as-prompts: `content` must tell the model how to proceed.
    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> Value;
    fn permission(&self, input: &Value) -> Permission;
    fn run<'a>(&'a self, input: Value, cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome>;
}

/// The human on the loop. Native CLI implements a y/n prompt; headless
/// default-denies (Sec #11). No allow-rule persistence exists until the
/// M1 sandbox floor (r2 R3) — every ask is asked.
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

pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    pub fn builtin() -> Self {
        Self::builtin_with(diagnostics::Diagnostics::default())
    }

    /// Builtins with post-mutation diagnostics (M3a) shared by edit/write.
    pub fn builtin_with(diag: diagnostics::Diagnostics) -> Self {
        let diag = std::sync::Arc::new(diag);
        Self {
            tools: vec![
                Box::new(ReadTool),
                Box::new(EditTool { diag: diag.clone() }),
                Box::new(WriteTool { diag }),
                Box::new(BashTool),
            ],
        }
    }

    /// Register an additional tool (MCP meta-tool, skills — M3).
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
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
        self.tools.iter().find(|t| t.name() == name).map(|b| b.as_ref())
    }
}

/// The execute-later class (0001 §M0): files whose *write* is benign-looking
/// but whose *execution* happens later, outside any gate. Writes here get the
/// escalated ask.
pub fn execute_later_reason(path: &str) -> Option<&'static str> {
    let p = path.trim_start_matches("./");
    let file = p.rsplit('/').next().unwrap_or(p);
    if p.contains(".git/hooks/") {
        return Some("git hook: runs on your next git command");
    }
    if matches!(file, "Makefile" | "makefile" | "GNUmakefile" | "justfile" | "Justfile") {
        return Some("build entrypoint: runs on your next make/just invocation");
    }
    if matches!(file, "AGENTS.md" | "CLAUDE.md") {
        return Some("agent instructions: injected into future model contexts");
    }
    if file == "settings.json" || p.contains(".hotl/") || p.contains(".claude/") {
        return Some("harness settings/hooks: change how future sessions behave");
    }
    if file.ends_with(".zshrc") || file.ends_with(".bashrc") || file.ends_with(".profile") {
        return Some("shell startup file: runs in every new shell");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_paths() {
        assert!(execute_later_reason(".git/hooks/pre-commit").is_some());
        assert!(execute_later_reason("sub/dir/Makefile").is_some());
        assert!(execute_later_reason("AGENTS.md").is_some());
        assert!(execute_later_reason(".hotl/settings.json").is_some());
        assert!(execute_later_reason("src/main.rs").is_none());
        assert!(execute_later_reason("docs/notes.md").is_none());
    }
}
