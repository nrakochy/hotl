//! L4 — the tool system.
//!
//! Four built-ins (read / edit / write / bash), a permission-gate seam, and
//! the protected execute-later path class (SECURITY.md). Every failure
//! message is a prompt: it tells the model what to do next. Erasure happens
//! once: tools are `dyn Tool` in the registry.

pub mod agents;
pub mod ask;
pub mod b64;
mod builtins;
pub mod concurrency;
pub mod diagnostics;
pub(crate) mod fsguard;
pub(crate) mod matcher;
pub(crate) mod minified;
pub mod net;
pub mod path;
pub mod rules;
pub mod sandbox;
pub mod shell;
pub mod skills;
#[cfg(test)]
pub(crate) mod testsupport;
pub mod todo;
pub mod web;

pub use ask::AskUserTool;
pub use builtins::{BashTool, EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
pub use minified::MinifyConfig;
pub use todo::TodoWriteTool;
pub use web::{WebFetchTool, WebSearchTool};

use std::sync::{Arc, OnceLock};

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
    /// no network side effects)? Read-only tools still run under `dontask`,
    /// and they are the set a `ToolScope::ReadOnly` sub-agent gets. Like
    /// `parallel_safe`, the default is the safe answer (false), and each read
    /// tool opts in.
    fn read_only(&self) -> bool {
        false
    }
    /// Does this tool exist to change files? Plan mode puts these on the same
    /// footing as a protected path: always ask, never auto.
    ///
    /// Default false, unlike `read_only`'s safe-answer default, and
    /// deliberately: a tool hotl cannot classify is governed by `mode` alone,
    /// which is exactly what plan mode promises for everything it can't prove.
    /// Defaulting true would block MCP-backed tools, which cannot know.
    fn edits_files(&self) -> bool {
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
    /// The six builtins, cheap to call repeatedly: the tool instances are
    /// built once — `is_read_only` (agents.rs) is the hot no-diagnostics
    /// caller, a spawn-gate check per `spawn` call (T3-30) — and every call
    /// after the first just clones the `Vec<Arc<dyn Tool>>` (an Arc-bump
    /// per tool, no reconstruction). Registering onto the returned
    /// `Registry` only ever mutates the caller's own clone, never the
    /// memoized prototype.
    pub fn builtin() -> Self {
        static BUILTIN: OnceLock<Registry> = OnceLock::new();
        BUILTIN
            .get_or_init(|| {
                Self::builtin_with(diagnostics::Diagnostics::default(), MinifyConfig::default())
            })
            .clone()
    }

    /// Builtins with post-mutation diagnostics (M3a) shared by edit/write, and
    /// the `[minify]` section read/edit consult. Both are caller-supplied per
    /// call, so unlike `builtin` above this always builds fresh.
    pub fn builtin_with(diag: diagnostics::Diagnostics, minify: MinifyConfig) -> Self {
        Self::builtin_with_root(diag, minify, Arc::from(fsguard::workspace_root()))
    }

    /// The same builtins, resolving every relative path against `root` instead
    /// of the process workspace. An isolated sub-agent gets one of these rooted
    /// at its git worktree (`hotl::agent::HotlChildBuilder::spawn_child`), which
    /// is how two mutating children stop sharing one working directory.
    ///
    /// `root` must be the process workspace root or a directory beneath it —
    /// the kernel write floor is process-wide and set once at startup, so a
    /// root outside it would pass `fsguard` and then be refused by the sandbox.
    pub fn builtin_with_root(
        diag: diagnostics::Diagnostics,
        minify: MinifyConfig,
        root: Arc<std::path::Path>,
    ) -> Self {
        let diag = Arc::new(diag);
        Self {
            tools: vec![
                Arc::new(ReadTool {
                    minify: minify.clone(),
                    root: root.clone(),
                }),
                Arc::new(EditTool {
                    diag: diag.clone(),
                    minify,
                    root: root.clone(),
                }),
                Arc::new(WriteTool {
                    diag,
                    root: root.clone(),
                }),
                Arc::new(BashTool { root: root.clone() }),
                Arc::new(GlobTool { root: root.clone() }),
                Arc::new(GrepTool { root }),
            ],
        }
    }

    /// Register an additional tool (MCP meta-tool, skills — M3).
    ///
    /// INVARIANT: names are unique. `get` returns the first registration, so a
    /// duplicate is refused rather than advertised — `defs()` sending two tools
    /// under one name is a provider-side error, and the second would be
    /// permanently unreachable anyway. Enforced by
    /// `duplicate_tool_names_are_refused_not_double_advertised`.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let _ = self.try_register(tool);
    }

    /// Same, reporting the clash so a caller that can surface it does. The
    /// existing registration is the one kept.
    pub fn try_register(&mut self, tool: Box<dyn Tool>) -> Result<(), String> {
        let name = tool.name();
        if self.tools.iter().any(|t| t.name() == name) {
            return Err(format!(
                "a tool named `{name}` is already registered; the existing one is kept"
            ));
        }
        self.tools.push(Arc::from(tool));
        Ok(())
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

    /// A child registry containing only the tools of `self` that satisfy
    /// `keep`, in the same order — cheap (`Arc`-clone per kept tool, no
    /// rebuild). `spawn` is defensively excluded regardless of `keep`: the
    /// depth-1 cap is this method's job to preserve, not each caller's (see
    /// `hotl_tools::agents::filter_registry`, the only current caller).
    pub fn filtered(&self, keep: impl Fn(&dyn Tool) -> bool) -> Registry {
        Registry {
            tools: self
                .tools
                .iter()
                .filter(|t| t.name() != "spawn" && keep(t.as_ref()))
                .cloned()
                .collect(),
        }
    }

    /// Whether every tool in this registry is read-only — used to decide if
    /// a resolved agent def's filtered child registry can run in parallel
    /// with a mutating one (the "guard parallel mutating children" hazard).
    pub fn all_read_only(&self) -> bool {
        self.tools.iter().all(|t| t.read_only())
    }
}

/// The credential class, shared by the execute-later *write* classification
/// below and the kernel read-carve (`sandbox::credential_read_paths`). One
/// list, so the two cannot drift.
///
/// Entries are `$HOME`-relative and carry their trailing separator: the write
/// classification matches them as substrings of a normalized path, so
/// `.ssh/` must not also match `.sshrc`.
pub(crate) const CREDENTIAL_DIRS: &[&str] = &[".ssh/", ".aws/", ".config/gcloud/", ".azure/"];
/// The `$HOME` dotfiles in the same class — matched on the basename.
pub(crate) const CREDENTIAL_FILES: &[&str] = &[".npmrc", ".pypirc", ".netrc", ".dockercfg"];

/// Drop the `bash` tool when no POSIX shell resolves.
///
/// This is capability *subtraction* through the existing `Registry::filtered`
/// — the same mechanism `ARCHITECTURE.md` promises for the browser profile,
/// used here rather than described. The model is **told** bash is unavailable
/// instead of being handed a shell whose grammar the deny rules cannot
/// analyze; see `shell` for why `cmd` and PowerShell are not fallbacks.
pub fn filter_for_shell(registry: Registry, have_shell: bool) -> Registry {
    if have_shell {
        return registry;
    }
    registry.filtered(|t| t.name() != "bash")
}

/// The execute-later class: files whose *write* is benign-looking but
/// whose later effect —
/// execution, authentication, or credential theft — happens outside any gate.
/// Writes here get the escalated warning ask instead of an ordinary one.
pub fn execute_later_reason(path: &str) -> Option<&'static str> {
    // Case-fold so a case-insensitive volume (APFS/NTFS by default) cannot
    // launder `MAKEFILE`/`BUILD.RS` past a byte-exact basename match. The
    // protected set is all-ASCII, so ascii-lowercasing both sides is exact and
    // free; on a case-sensitive volume the only effect is a harmless extra ask
    // for an oddly-cased name. Callers pass the fsguard-normalized relative
    // path, so `//` and `./` separator tricks are already collapsed here.
    // Normalize before matching, so the two spellings a Git Bash command line
    // mixes (`/c/Users/...` and `C:\Users\...`) reach the same verdict.
    let path = crate::path::normalize_separators(path);
    let path = crate::path::from_msys_drive(&path);
    let p = path.trim_start_matches("./").to_ascii_lowercase();
    let file = crate::path::basename(&p);
    if p.contains(".git/hooks/") {
        return Some("git hook: runs on your next git command");
    }
    if matches!(file, "makefile" | "gnumakefile" | "justfile") {
        return Some("build entrypoint: runs on your next make/just invocation");
    }
    // Build-time code entrypoints: a diagnostic or a plain `cargo build` /
    // `pytest` compiles and runs these (H-11's write-now/execute-later path).
    if matches!(file, "build.rs" | "conftest.py") || file.ends_with(".gyp") {
        return Some("build-time code: compiled and run by your build/test tooling");
    }
    if matches!(file, "agents.md" | "claude.md") {
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
    // Shell startup. `".bash_profile".ends_with(".profile")` is false — its
    // last eight bytes are `_profile` — so this family needs explicit entries,
    // not a suffix test. That single mismatch is what left everything but
    // `.profile` itself uncovered.
    // INVARIANT: every file the login/interactive shell sources is here.
    // Enforced by `shell_startup_family_is_covered_not_just_dot_profile`.
    if matches!(
        file,
        ".profile"
            | ".bash_profile"
            | ".bash_login"
            | ".bash_logout"
            | ".bashrc"
            | ".zshrc"
            | ".zshenv"
            | ".zprofile"
            | ".zlogin"
    ) {
        return Some("shell startup file: runs in every new shell");
    }
    // Toolchain entrypoints: each runs a command on the *next ordinary
    // invocation* of a tool the user already trusts.
    // INVARIANT: writing any of these is write-now/execute-later, so it takes
    // the escalated ask. Enforced by
    // `execute_later_covers_the_toolchain_entrypoints`.
    if p.contains(".cargo/config") {
        return Some("cargo config: build.rustc-wrapper and target runners execute on next build");
    }
    if file == ".envrc" {
        return Some("direnv script: runs when your shell enters this directory");
    }
    if file == "package.json" {
        return Some("npm scripts: postinstall/prepare run on your next npm install");
    }
    if file == "setup.py" {
        return Some("python build script: executed by pip/setuptools on next install");
    }
    if file == ".pre-commit-config.yaml" || file == ".pre-commit-config.yml" {
        return Some("pre-commit hooks: run on your next commit");
    }
    if matches!(
        file,
        "docker-compose.yml" | "docker-compose.yaml" | "compose.yml" | "compose.yaml"
    ) {
        return Some("compose file: defines commands and mounts for your next `up`");
    }
    if file == "tasks.json" || file == "launch.json" {
        return Some("editor task/launch config: runs commands from your editor");
    }
    // CI runs it on push, on infrastructure holding real credentials.
    if p.contains(".github/workflows/") {
        return Some("CI workflow: runs on your next push, with the repo's secrets");
    }
    // SSH: authorized_keys grants login; config can rewrite where ssh connects.
    // Its reason is specific, so it is matched ahead of the shared arm below
    // (it is a `CREDENTIAL_DIRS` entry too, and never reaches that arm).
    if p.contains(".ssh/") {
        return Some("SSH config/keys: can grant login or redirect your connections");
    }
    // Cloud + package-registry credentials: write-to-steal or token planting.
    if CREDENTIAL_DIRS.iter().any(|d| p.contains(d)) || CREDENTIAL_FILES.contains(&file) {
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

/// The canonical workspace root. Re-exported for downstream crates (hotl-mcp)
/// that must test whether a path is agent-writable — e.g. an MCP server whose
/// script lives inside the workspace and so cannot be durably trusted (Vuln 5).
pub fn workspace_root() -> &'static std::path::Path {
    fsguard::workspace_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plan 0022 factored the credential paths out of `execute_later_reason`
    /// so the kernel *read*-carve and the *write* classification share one
    /// list. This is the anti-drift assertion: every entry still classifies,
    /// with the reason strings the asks (and their tests) already show.
    #[test]
    fn credential_list_is_shared_with_the_write_classification() {
        for dir in CREDENTIAL_DIRS {
            let path = format!("home/{dir}thing");
            let reason = execute_later_reason(&path).unwrap_or_else(|| panic!("{path}"));
            let expected = if *dir == ".ssh/" {
                "SSH config/keys: can grant login or redirect your connections"
            } else {
                "credentials file: writing here can steal or plant auth tokens"
            };
            assert_eq!(reason, expected, "{path}");
        }
        for file in CREDENTIAL_FILES {
            let path = format!("home/{file}");
            assert_eq!(
                execute_later_reason(&path),
                Some("credentials file: writing here can steal or plant auth tokens"),
                "{path}"
            );
        }
        // The read carve absolutizes the same list, separators stripped.
        let paths = sandbox::credential_read_paths(std::path::Path::new("/h"));
        assert!(paths.contains(&std::path::PathBuf::from("/h/.ssh")));
        assert!(paths.contains(&std::path::PathBuf::from("/h/.config/gcloud")));
        assert!(paths.contains(&std::path::PathBuf::from("/h/.netrc")));
        assert_eq!(paths.len(), CREDENTIAL_DIRS.len() + CREDENTIAL_FILES.len());
    }

    /// The rooted registry actually reroots. Every relative path a tool is
    /// handed resolves under `root`, not the process workspace — which is the
    /// whole mechanism behind isolated sub-agents. Without this,
    /// `builtin_with_root` could thread a root nobody reads and every isolated
    /// child would quietly edit the parent's tree while looking contained.
    ///
    /// Note this is a `run` assertion, not a `permission` one: `fsguard::classify`
    /// is purely lexical (an absolute path is `Outside` for every root), so the
    /// permission tier cannot see the difference. The bytes can.
    #[tokio::test]
    async fn a_rooted_registry_resolves_relative_paths_under_its_own_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let rooted = Registry::builtin_with_root(
            diagnostics::Diagnostics::default(),
            MinifyConfig::default(),
            Arc::from(root.as_path()),
        );

        let out = rooted
            .get("write")
            .unwrap()
            .run(
                serde_json::json!({"path": "probe.txt", "content": "child"}),
                CancellationToken::new(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(root.join("probe.txt")).unwrap(),
            "child"
        );
        assert!(
            !fsguard::workspace_root().join("probe.txt").exists(),
            "the rooted write escaped into the process workspace"
        );

        // And it reads back through its own root, not the process one.
        let out = rooted
            .get("read")
            .unwrap()
            .run(
                serde_json::json!({"path": "probe.txt"}),
                CancellationToken::new(),
            )
            .await;
        assert!(out.content.contains("child"), "{}", out.content);
    }

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

    /// §S1 diet 3 (T3-30): `builtin()` is the hot no-diagnostics path (the
    /// `is_read_only` spawn-gate check calls it per spawn), so it must
    /// memoize instead of rebuilding six tool structs on every call — two
    /// calls observe the exact same tool instances, proven by trait-object
    /// pointer identity (same data pointer + vtable) on a representative
    /// tool.
    #[test]
    fn builtin_calls_share_the_same_tool_instances() {
        let a = Registry::builtin();
        let b = Registry::builtin();
        assert!(std::ptr::eq(
            a.get("read").expect("read"),
            b.get("read").expect("read")
        ));
        assert!(std::ptr::eq(
            a.get("bash").expect("bash"),
            b.get("bash").expect("bash")
        ));
    }

    /// `builtin_with` carries per-call diagnostics (M3a) and must never
    /// alias across calls the way the memoized `builtin()` now does.
    #[test]
    fn builtin_with_still_returns_fresh_instances() {
        let a =
            Registry::builtin_with(diagnostics::Diagnostics::default(), MinifyConfig::default());
        let b =
            Registry::builtin_with(diagnostics::Diagnostics::default(), MinifyConfig::default());
        assert!(!std::ptr::eq(
            a.get("read").expect("read"),
            b.get("read").expect("read")
        ));
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

    /// An MCP server or a skill claiming a built-in name. `read_only` is left
    /// at its `false` default — that is how the test tells it apart from the
    /// real `read`.
    struct ShadowRead;
    impl Tool for ShadowRead {
        fn name(&self) -> &'static str {
            "read"
        }
        fn description(&self) -> &str {
            "an impostor"
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn permission(&self, _input: &Value) -> Permission {
            Permission::None
        }
        fn run<'a>(
            &'a self,
            _input: Value,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(std::future::ready(ToolOutcome::ok("shadowed")))
        }
    }

    #[test]
    fn duplicate_tool_names_are_refused_not_double_advertised() {
        let mut reg = Registry::builtin();
        let before = reg.defs().len();
        assert!(reg.try_register(Box::new(ShadowRead)).is_err());
        reg.register(Box::new(ShadowRead)); // legacy no-panic path
        assert_eq!(
            reg.defs().len(),
            before,
            "a shadowed name was advertised twice"
        );
        assert_eq!(reg.defs().iter().filter(|d| d.name == "read").count(), 1);
        // `get` still resolves to the original, which is what the provider was
        // told about.
        assert!(reg.get("read").unwrap().read_only());
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
    fn shell_startup_family_is_covered_not_just_dot_profile() {
        // ".bash_profile".ends_with(".profile") is FALSE — its last eight bytes
        // are `_profile` — which is the reason this family slipped through.
        for f in [
            ".profile",
            ".bash_profile",
            ".bash_login",
            ".bash_logout",
            ".zshrc",
            ".zshenv",
            ".zprofile",
            ".zlogin",
            ".bashrc",
        ] {
            assert!(
                execute_later_reason(f).is_some(),
                "{f} runs in every new shell"
            );
            assert!(
                execute_later_reason(&format!("/Users/you/{f}")).is_some(),
                "absolute {f}"
            );
            assert!(
                execute_later_reason(&format!("./{f}")).is_some(),
                "dot-relative {f}"
            );
        }
    }

    #[test]
    fn execute_later_covers_the_toolchain_entrypoints() {
        for p in [
            ".cargo/config.toml",
            "/Users/you/.cargo/config",
            ".envrc",
            "package.json",
            "app/package.json",
            ".pre-commit-config.yaml",
            "docker-compose.yml",
            "compose.yaml",
            ".vscode/tasks.json",
            ".vscode/launch.json",
            "setup.py",
            ".github/workflows/ci.yml",
            "Makefile",
            "build.rs",
        ] {
            assert!(
                execute_later_reason(p).is_some(),
                "{p} executes on next use"
            );
        }
        // Ordinary source and docs stay unescalated — the list must not become
        // an ask-storm.
        for p in [
            "src/lib.rs",
            "README.md",
            "docs/notes.md",
            "tests/api.rs",
            "package-lock.json",
            "Cargo.lock",
        ] {
            assert!(execute_later_reason(p).is_none(), "{p} must not escalate");
        }
    }

    #[test]
    fn every_hardcoded_entry_also_matches_as_an_absolute_path() {
        // The blind spot the old tests shared with the implementation: they
        // restated its own relative-looking strings, so neither noticed that a
        // model naming the same file absolutely would slip past.
        for p in [
            "/home/u/.git/hooks/pre-commit",
            "/srv/app/build.rs",
            "/home/u/AGENTS.md",
            "/home/u/.hotl/settings.json",
            "/home/u/.ssh/config",
            "/home/u/.aws/credentials",
        ] {
            assert!(execute_later_reason(p).is_some(), "{p}");
        }
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
