//! The execute surface, headless: `-p` one-shot and `--json-schema`
//! structured runs. The interactive console is the TUI (crates/hotl-tui +
//! tui.rs); this module also hosts the engine scaffolding the TUI, ACP, and
//! the socket server share (`acp_factory`, config/session paths, providers).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use hotl_context::{load_memory, load_system_prompt, project_instructions};
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::{Clock, EnvSecrets, SecretStore, SystemClock};
use hotl_provider_anthropic::{AnthropicProvider, DEFAULT_MODEL};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, sandbox, Registry};
use tokio::signal::unix::{signal, SignalKind};

/// Stable schema version of the `-p --json` event stream (MD Tier-1 contract;
/// bump only on a breaking change to a frame's shape).
pub const JSON_STREAM_SCHEMA_VERSION: u32 = 1;

/// Context inherited from an earlier session (`hotl resume` — M3b).
pub(crate) struct Resumed {
    pub parent_id: String,
    pub items: Vec<hotl_types::Item>,
    /// The parent's last `ModeSet`, if any (durable, last-wins — same
    /// inheritance shape as the display name). `None` = the parent never
    /// left its startup default, so the resumed session keeps its own.
    pub mode: Option<String>,
}

pub async fn agent_main(args: Vec<String>) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    match (parsed.schema, parsed.prompt) {
        (Some(schema), Some(prompt)) => structured_main(&prompt, &schema, parsed.name).await,
        (None, Some(prompt)) => run_session(prompt, parsed.json_events, parsed.name).await,
        // Reachable via e.g. `hotl --json` with no -p (main.rs routes any
        // headless flag here); the interactive console is bare `hotl`.
        (_, None) => {
            eprintln!(
                "hotl: -p \"prompt\" is required headless — the interactive console is bare `hotl` in a terminal"
            );
            2
        }
    }
}

/// `hotl -p "…" --json-schema <file>` (T2): run one headless turn, validate the
/// answer against the schema (with bounded retry), print the JSON or exit 1.
async fn structured_main(prompt: &str, schema_path: &std::path::Path, name: Option<String>) -> i32 {
    let schema: serde_json::Value = match std::fs::read_to_string(schema_path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "hotl: could not read --json-schema `{}`: {e}",
                schema_path.display()
            );
            return 2;
        }
    };
    let secrets = EnvSecrets;
    let cfg = crate::config::Config::load(&config_dir());
    let (provider, model, key_source) = match select_provider(&cfg, &secrets) {
        Ok(triple) => triple,
        Err(msg) => {
            eprintln!("hotl: {msg}");
            return 1;
        }
    };
    let scaffold = match scaffold(provider, model, &secrets, cfg, key_source).await {
        Ok(s) => s,
        Err(code) => return code,
    };
    let mut log = match SessionLog::create(
        &sessions_dir(),
        &scaffold.model,
        None,
        scaffold.masker(),
        scaffold.clock.now_ms(),
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl: could not create session log: {e}");
            return 1;
        }
    };
    if let Some(n) = &name {
        let _ = log.append(
            &hotl_types::EntryPayload::Rename { name: n.clone() },
            scaffold.clock.now_ms(),
        );
    }
    let mut items = initial_items(&scaffold.config_dir, &scaffold.cwd);
    items.push(crate::structured::contract_item(&schema));
    let mut handle = spawn_session(scaffold.deps(log, None, items, None));
    match crate::structured::run_structured(
        &mut handle,
        &schema,
        prompt,
        crate::structured::MAX_RETRIES,
    )
    .await
    {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(e) => {
            eprintln!("hotl: {e}");
            1
        }
    }
}

/// `hotl acp`: serve the ACP JSON-RPC protocol over stdio (M4). Wires the
/// real engine deps into a session factory and hands the streams to the
/// protocol loop. One connection, one process (process-per-session).
pub async fn acp_main() -> i32 {
    let (factory, _model, skills) = match acp_factory().await {
        Ok(triple) => triple,
        Err(code) => return code,
    };
    crate::acp::serve(tokio::io::stdin(), tokio::io::stdout(), factory, skills).await;
    0
}

/// The real-engine session factory `hotl acp` and `hotl tui` share, plus the
/// resolved model name. Prints its own errors; `Err` carries the exit code.
pub(crate) async fn acp_factory() -> Result<(crate::acp::SessionFactory, String, Vec<String>), i32>
{
    let secrets = EnvSecrets;
    let cfg = crate::config::Config::load(&config_dir());
    let (provider, model, key_source) = match select_provider(&cfg, &secrets) {
        Ok(triple) => triple,
        Err(msg) => {
            eprintln!("hotl: {msg}");
            return Err(1);
        }
    };
    let scaffold = match scaffold(provider, model, &secrets, cfg, key_source).await {
        Ok(s) => s,
        Err(code) => return Err(code),
    };
    let model = scaffold.model.clone();
    let skill_names = scaffold.skill_names.clone();
    let factory: crate::acp::SessionFactory = Box::new(move |spec| {
        let (resumed, requested) = match spec {
            crate::acp::SessionSpec::New { name } => (None, name),
            crate::acp::SessionSpec::Load {
                session_id: sid,
                name,
            } => {
                let replayed = hotl_store::replay_chain(&sessions_dir(), &sid)
                    .map_err(|e| format!("could not load session {sid}: {e}"))?;
                let hotl_store::Replayed {
                    header,
                    items,
                    name: inherited,
                    mode,
                    ..
                } = replayed;
                // An explicit rename-on-resume beats the inherited name.
                let name = name.or(inherited);
                (
                    Some(Resumed {
                        parent_id: header.session_id,
                        items,
                        mode,
                    }),
                    name,
                )
            }
        };
        let parent_id = resumed.as_ref().map(|r| r.parent_id.clone());
        let mut log = SessionLog::create(
            &sessions_dir(),
            &scaffold.model,
            parent_id,
            scaffold.masker(),
            scaffold.clock.now_ms(),
        )
        .map_err(|e| format!("could not create session log: {e}"))?;
        // Copy-forward: the resumed name lives in this log too, so listing
        // and name resolution stay a single-file scan.
        if let Some(n) = &requested {
            let _ = log.append(
                &hotl_types::EntryPayload::Rename { name: n.clone() },
                scaffold.clock.now_ms(),
            );
        }
        // Copy-forward the inherited mode too (same reasoning as the name):
        // this log is now the single-file source of truth for `hotl resume`.
        // An unrecognized mode string (a future build's mode this binary
        // doesn't know) copies forward as history but never overrides —
        // `mode_override` stays `None`, so the session keeps its own default.
        let inherited_mode = resumed.as_ref().and_then(|r| r.mode.clone());
        let mode_override = inherited_mode
            .as_deref()
            .and_then(hotl_tools::rules::PermissionMode::from_str);
        if let Some(m) = inherited_mode {
            let _ = log.append(
                &hotl_types::EntryPayload::ModeSet { mode: m },
                scaffold.clock.now_ms(),
            );
        }
        let session_id = log.session_id.clone();
        let (snapshots, initial) =
            session_context(&session_id, &scaffold.cwd, &scaffold.config_dir, &resumed);
        Ok(crate::acp::SessionOpen {
            handle: spawn_session(scaffold.deps(log, snapshots, initial, mode_override)),
            name: requested,
        })
    });
    Ok((factory, model, skill_names))
}

/// `hotl serve --id <id> [--prompt <p>]`: build a session and host it on a
/// unix socket for `hotl attach` (the detached-session server behind `hotl bg`).
pub async fn serve_main(id: String, prompt: Option<String>, name: Option<String>) -> i32 {
    let secrets = EnvSecrets;
    let cfg = crate::config::Config::load(&config_dir());
    let (provider, model, key_source) = match select_provider(&cfg, &secrets) {
        Ok(triple) => triple,
        Err(msg) => {
            eprintln!("hotl serve: {msg}");
            return 1;
        }
    };
    let scaffold = match scaffold(provider, model, &secrets, cfg, key_source).await {
        Ok(s) => s,
        Err(code) => return code,
    };
    let mut log = match SessionLog::create(
        &sessions_dir(),
        &scaffold.model,
        None,
        scaffold.masker(),
        scaffold.clock.now_ms(),
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl serve: could not create session log: {e}");
            return 1;
        }
    };
    if let Some(n) = &name {
        let _ = log.append(
            &hotl_types::EntryPayload::Rename { name: n.clone() },
            scaffold.clock.now_ms(),
        );
    }
    let session_id = log.session_id.clone();
    let (snapshots, initial_items) =
        session_context(&session_id, &scaffold.cwd, &scaffold.config_dir, &None);
    let handle = spawn_session(scaffold.deps(log, snapshots, initial_items, None));
    crate::session_server::serve(id, handle, prompt).await
}

/// The deps every session shares (provider, registry-with-spawn, rules, hooks,
/// config, sandbox, cwd). Built once per process; `deps()` stamps a per-session
/// log, snapshots, and initial items onto it.
struct Scaffold {
    provider: Arc<dyn hotl_provider::Provider>,
    model: String,
    clock: Arc<dyn Clock>,
    config_dir: PathBuf,
    system: String,
    rules: Arc<Rules>,
    sandbox_enforced: bool,
    cwd: PathBuf,
    config: EngineConfig,
    registry: Arc<Registry>,
    /// Loadable skill names, produced by the registry's own discovery walk
    /// so nothing walks the skill roots a second time.
    skill_names: Vec<String>,
    hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
    /// The api-key-helper's key, acquired once at startup validation below.
    /// `None` for a static key source (nothing to register: it's already a
    /// process env var and `Masker::from_env()` already covers it).
    initial_helper_key: Option<String>,
}

/// Builds the process-wide scaffold, validating `key_source` first: a broken
/// helper fails here, with its own message, before any session log or
/// registry exists — not mid-turn.
async fn scaffold(
    provider: Arc<dyn hotl_provider::Provider>,
    model: String,
    secrets: &dyn SecretStore,
    cfg: crate::config::Config,
    key_source: Arc<dyn hotl_provider::key::KeySource>,
) -> Result<Scaffold, i32> {
    let initial_helper_key = match key_source.get().await {
        Ok(k) => k.filter(|_| key_source.refreshable()),
        Err(e) => {
            eprintln!("hotl: {e}");
            return Err(1);
        }
    };
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let config_dir = config_dir();
    // config.toml [behavior].sandbox = false disables the floor (env still wins).
    if cfg.behavior.sandbox == Some(false) && secrets.get("HOTL_SANDBOX").is_none() {
        std::env::set_var("HOTL_SANDBOX", "off");
    }
    let system = load_system_prompt(&config_dir);
    let rules = load_rules(&cfg);
    let sandbox_status = sandbox::probe();
    // [network] egress policy — installed process-wide (set-once) before any
    // command can run; child sessions inherit it via the global, and nothing
    // downstream can re-init it back to Open.
    let (egress_policy, egress_warning) = cfg.network.egress_policy();
    if let Some(warning) = &egress_warning {
        eprintln!("hotl: WARNING — {warning}");
    }
    hotl_tools::net::init(egress_policy);
    // Bash auto-allow needs the whole posture honest: the write floor
    // enforced AND any configured egress restriction kernel-backed (a policy
    // the kernel can't enforce drops bash rules back to asks, mirroring the
    // UNSANDBOXED carve-out).
    let sandbox_enforced = matches!(sandbox_status, sandbox::SandboxStatus::Enforced(_))
        && hotl_tools::net::auto_allow_permitted(&sandbox_status);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = engine_config(&model, secrets, &cfg);
    let spawn_builder = child_builder(
        provider.clone(),
        rules.clone(),
        clock.clone(),
        config.clone(),
        cwd.clone(),
        cfg.hooks_toml(),
        system.clone(),
        model.clone(),
        sandbox_enforced,
        initial_helper_key.clone(),
    );
    let (registry, skill_names) = build_registry(&cfg, &config_dir, Some(spawn_builder));
    let registry = Arc::new(registry);
    let hooks = load_hooks(&cfg);
    Ok(Scaffold {
        provider,
        model,
        clock,
        config_dir,
        system,
        rules,
        sandbox_enforced,
        cwd,
        config,
        registry,
        skill_names,
        hooks,
        initial_helper_key,
    })
}

impl Scaffold {
    /// Session masker: env-named secrets plus the helper-acquired key.
    /// Refreshed keys are NOT re-registered: keys never enter log entries;
    /// this registration is defense-in-depth for the startup key.
    pub(crate) fn masker(&self) -> Masker {
        masker_with_helper(self.initial_helper_key.as_deref())
    }

    /// `mode_override` seeds a resumed session's *starting* effective mode
    /// from its own history (the copy-forward `ModeSet`) instead of the
    /// process-wide startup default — a per-session `Rules` clone, not a
    /// mutation of the shared one (every other session in this process must
    /// keep its own default).
    fn deps(
        &self,
        log: SessionLog,
        snapshots: Option<Arc<dyn hotl_engine::Snapshotter>>,
        initial_items: Vec<hotl_types::Item>,
        mode_override: Option<hotl_tools::rules::PermissionMode>,
    ) -> SessionDeps {
        let rules = match mode_override {
            Some(m) => Arc::new((*self.rules).clone().with_mode(m)),
            None => self.rules.clone(),
        };
        SessionDeps {
            provider: self.provider.clone(),
            registry: self.registry.clone(),
            rules,
            sandbox_enforced: self.sandbox_enforced,
            clock: self.clock.clone(),
            log,
            system: self.system.clone(),
            cwd: self.cwd.clone(),
            snapshots,
            hooks: self.hooks.clone(),
            initial_items,
            config: self.config.clone(),
        }
    }
}

/// Env-named secrets plus, when a helper minted this process's key, that
/// value too — it never appears as a process env var, so `Masker::from_env()`
/// alone would miss it.
fn masker_with_helper(initial_helper_key: Option<&str>) -> Masker {
    match initial_helper_key {
        Some(k) => Masker::from_env().with_value("HOTL_API_KEY_HELPER", k),
        None => Masker::from_env(),
    }
}

async fn run_session(prompt: String, json_events: bool, name: Option<String>) -> i32 {
    let secrets = EnvSecrets;
    let cfg = crate::config::Config::load(&config_dir());
    let (provider, model, key_source) = match select_provider(&cfg, &secrets) {
        Ok(triple) => triple,
        Err(msg) => {
            eprintln!("hotl: {msg}");
            return 1;
        }
    };
    let scaffold = match scaffold(provider, model, &secrets, cfg, key_source).await {
        Ok(s) => s,
        Err(code) => return code,
    };

    let mut log = match SessionLog::create(
        &sessions_dir(),
        &scaffold.model,
        None,
        scaffold.masker(),
        scaffold.clock.now_ms(),
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl: could not create session log: {e}");
            return 1;
        }
    };
    if let Some(n) = &name {
        let _ = log.append(
            &hotl_types::EntryPayload::Rename { name: n.clone() },
            scaffold.clock.now_ms(),
        );
    }
    let session_id = log.session_id.clone();
    spawn_secret_audit(log.path().to_path_buf());
    let gc_config_dir = scaffold.config_dir.clone();
    std::thread::spawn(move || crate::gc::auto_gc(&gc_config_dir)); // retention, off the hot path
    let (snapshots, initial_items) =
        session_context(&session_id, &scaffold.cwd, &scaffold.config_dir, &None);
    let handle = spawn_session(scaffold.deps(log, snapshots, initial_items, None));

    let mut surface = Surface::new(handle, json_events);
    surface
        .handle
        .prompt(crate::setup::expand_file_refs(&prompt))
        .await;
    surface.run_until_idle().await
}

/// Builtins + the `mcp` meta-tool (M3a) + the `spawn` tool (M4) when a child
/// builder is supplied. `spawn` is omitted for child sessions, so sub-agents
/// cannot recurse (structural depth cap).
fn build_registry(
    cfg: &crate::config::Config,
    config_dir: &std::path::Path,
    spawn_builder: Option<Arc<dyn crate::spawn::ChildBuilder>>,
) -> (Registry, Vec<String>) {
    // Everything is config.toml: [diagnostics] and [[mcp]] sections.
    let diagnostics = cfg
        .hooks_toml()
        .map(|t| hotl_tools::diagnostics::Diagnostics::from_toml(&t))
        .unwrap_or_default();
    let mut registry = Registry::builtin_with(diagnostics);
    let servers = cfg
        .mcp_toml()
        .and_then(|t| toml::from_str::<hotl_mcp::config::McpConfig>(&t).ok())
        .map(|c| c.servers)
        .unwrap_or_default();
    if !servers.is_empty() {
        let trust = hotl_mcp::trust::TrustStore::load(config_dir);
        registry.register(Box::new(hotl_mcp::McpTool::new(servers, trust)));
    }
    // Claude Code skills (SKILL.md roots) load alongside hotl's own unless
    // opted out via [skills] claude = false; [skills.marketplaces] roots
    // are hotl's own and load regardless.
    let include_claude = cfg.skills.claude.unwrap_or(true);
    let (marketplaces, warnings) = cfg.skills.marketplace_roots(config_dir);
    for w in warnings {
        eprintln!("hotl: {w}");
    }
    // One discovery walk: the names for `/`-dispatch come off the same
    // tool that goes into the registry, never a second scan of the roots.
    let mut skill_names = Vec::new();
    if let Some(skills) =
        hotl_tools::skills::SkillTool::new(config_dir, include_claude, &marketplaces)
    {
        skill_names = skills.names().map(String::from).collect();
        registry.register(Box::new(skills));
    }
    // Retrieval backends (`[[retrieval]]`) → the `recall` tool. Absent when
    // nothing is configured: no ambient context cost when unused.
    let retrieval = cfg
        .retrieval_toml()
        .and_then(|t| toml::from_str::<hotl_retrieval::config::RetrievalConfig>(&t).ok())
        .map(|c| c.backends)
        .unwrap_or_default();
    if !retrieval.is_empty() {
        let (backends, warnings) = hotl_retrieval::config::build(retrieval, config_dir);
        for w in warnings {
            eprintln!("hotl: {w}");
        }
        if !backends.is_empty() {
            registry.register(Box::new(hotl_retrieval::RecallTool::new(backends)));
        }
    }
    if let Some(builder) = spawn_builder {
        registry.register(Box::new(crate::spawn::SpawnTool::new(builder)));
    }
    (registry, skill_names)
}

/// A `ChildBuilder` that spawns an isolated sub-agent sharing the parent's
/// provider/rules/config but with a builtins-only registry (no spawn, no MCP,
/// no snapshots — a clean, non-recursive child). M4.
struct HotlChildBuilder {
    provider: Arc<dyn hotl_provider::Provider>,
    rules: Arc<Rules>,
    clock: Arc<dyn Clock>,
    config: EngineConfig,
    cwd: PathBuf,
    /// The parent's config.toml `[diagnostics]` (as a hooks.toml-shaped
    /// string), captured at construction — children don't re-read the file.
    hooks_toml: Option<String>,
    system: String,
    model: String,
    sandbox_enforced: bool,
    /// See `Scaffold::initial_helper_key` — passed down at construction since
    /// a child builder is captured by the spawn tool ahead of any session.
    initial_helper_key: Option<String>,
}

impl HotlChildBuilder {
    /// Same masking as `Scaffold::masker` — a child session can echo the
    /// same acquired key into its own log.
    fn masker(&self) -> Masker {
        masker_with_helper(self.initial_helper_key.as_deref())
    }
}

impl crate::spawn::ChildBuilder for HotlChildBuilder {
    fn build(&self, _brief: &str) -> Result<hotl_engine::SessionHandle, String> {
        let log = SessionLog::create(
            &sessions_dir(),
            &self.model,
            None,
            self.masker(),
            self.clock.now_ms(),
        )
        .map_err(|e| format!("child session log: {e}"))?;
        let diagnostics = self
            .hooks_toml
            .as_deref()
            .map(hotl_tools::diagnostics::Diagnostics::from_toml)
            .unwrap_or_default();
        let registry = Registry::builtin_with(diagnostics);
        Ok(spawn_session(SessionDeps {
            provider: self.provider.clone(),
            registry: Arc::new(registry),
            rules: self.rules.clone(),
            sandbox_enforced: self.sandbox_enforced,
            clock: self.clock.clone(),
            log,
            system: self.system.clone(),
            cwd: self.cwd.clone(),
            snapshots: None,
            hooks: None,
            initial_items: Vec::new(),
            config: self.config.clone(),
        }))
    }
}

#[allow(clippy::too_many_arguments)]
fn child_builder(
    provider: Arc<dyn hotl_provider::Provider>,
    rules: Arc<Rules>,
    clock: Arc<dyn Clock>,
    config: EngineConfig,
    cwd: PathBuf,
    hooks_toml: Option<String>,
    system: String,
    model: String,
    sandbox_enforced: bool,
    initial_helper_key: Option<String>,
) -> Arc<dyn crate::spawn::ChildBuilder> {
    Arc::new(HotlChildBuilder {
        provider,
        rules,
        clock,
        config,
        cwd,
        hooks_toml,
        system,
        model,
        sandbox_enforced,
        initial_helper_key,
    })
}

/// Snapshotter + starting context for a session. A resumed session inherits
/// the replayed projection verbatim (it already carries the original memory
/// and instructions); fresh sessions assemble anew.
fn session_context(
    session_id: &str,
    cwd: &std::path::Path,
    config_dir: &std::path::Path,
    resumed: &Option<Resumed>,
) -> (
    Option<Arc<dyn hotl_engine::Snapshotter>>,
    Vec<hotl_types::Item>,
) {
    let snapshots = shadow_snapshotter(session_id, cwd);
    if snapshots.is_none() {
        eprintln!("hotl: git not found — `hotl undo` snapshots disabled this session");
    }
    let items = match resumed {
        Some(r) => r.items.clone(),
        None => initial_items(config_dir, cwd),
    };
    (snapshots, items)
}

/// Shadow-git snapshotter (M3b): blocking git work runs on the blocking
/// pool so a slow snapshot never stalls the turn.
struct GitSnapshotter(Arc<hotl_store::shadow::Shadow>);

impl hotl_engine::Snapshotter for GitSnapshotter {
    fn snapshot(&self, label: String) -> futures_util::future::BoxFuture<'static, ()> {
        let shadow = self.0.clone();
        Box::pin(async move {
            let _ = tokio::task::spawn_blocking(move || shadow.snapshot(&label)).await;
        })
    }
}

fn shadow_snapshotter(
    session_id: &str,
    cwd: &std::path::Path,
) -> Option<Arc<dyn hotl_engine::Snapshotter>> {
    let shadow = hotl_store::shadow::Shadow::create(&shadow_root(), session_id, cwd)?;
    Some(Arc::new(GitSnapshotter(Arc::new(shadow))))
}

pub(crate) fn shadow_root() -> PathBuf {
    sessions_dir()
        .parent()
        .map(|p| p.join("shadow"))
        .unwrap_or_else(|| PathBuf::from("shadow"))
}

/// `hotl undo [--force]`: restore the workspace to the newest session's
/// last pre-batch snapshot. Interactive confirm unless --force.
pub(crate) fn undo_main(args: Vec<String>) -> i32 {
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    let root = shadow_root();
    let Some(session) = hotl_store::shadow::latest_session(&root) else {
        eprintln!("hotl: no shadow snapshots found (sessions record them automatically when git is available)");
        return 1;
    };
    let Some(shadow) = hotl_store::shadow::Shadow::open(&root, &session) else {
        eprintln!("hotl: shadow repo for session {session} is unreadable");
        return 1;
    };
    let Some((hash, label)) = shadow.latest_pre() else {
        eprintln!("hotl: session {session} has no pre-batch snapshot to restore");
        return 1;
    };
    println!(
        "restore `{}` to snapshot \"{label}\" of session {session}?",
        shadow.work_tree().display()
    );
    if !force {
        eprint!("this overwrites tracked files changed since then [y/N] ");
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(answer.trim(), "y" | "Y" | "yes")
        {
            println!("(cancelled)");
            return 1;
        }
    }
    match shadow.restore(&hash) {
        Ok(files) if files.is_empty() => {
            println!("nothing differed — tree already matches \"{label}\"");
            0
        }
        Ok(files) => {
            println!("restored {} file(s) to \"{label}\":", files.len());
            for f in &files {
                println!("  {f}");
            }
            println!("(files created after the snapshot are kept, listed above if changed)");
            0
        }
        Err(e) => {
            eprintln!("hotl: undo failed: {e}");
            1
        }
    }
}

/// Lane-2 shell hooks from config.toml `[[hook]]`, or None (M5).
fn load_hooks(cfg: &crate::config::Config) -> Option<Arc<dyn hotl_engine::hooks::Hooks>> {
    cfg.hooks_toml()
        .and_then(|t| crate::shell_hooks::load_str(&t))
        .map(|h| Arc::new(h) as Arc<dyn hotl_engine::hooks::Hooks>)
}

pub(crate) const ADMIN_RULES_PATH: &str = "/etc/hotl/preapproved.toml";

/// Allow/deny rules from config.toml plus the admin tier, with the resolved
/// permission mode. Prints its startup warnings — posture never changes
/// silently.
fn load_rules(cfg: &crate::config::Config) -> Arc<Rules> {
    let admin_path = std::env::var("HOTL_PREAPPROVED").unwrap_or_else(|_| ADMIN_RULES_PATH.into());
    let env_mode = std::env::var("HOTL_PERMISSIONS").ok();
    let (rules, warnings) = load_rules_with(
        cfg,
        Some(std::path::Path::new(&admin_path)),
        env_mode.as_deref(),
    );
    for w in warnings {
        eprintln!("hotl: {w}");
    }
    rules
}

/// The testable core of [`load_rules`]: explicit admin path + env mode, no
/// process-global reads. Returns the rules and the warnings to print.
fn load_rules_with(
    cfg: &crate::config::Config,
    admin_path: Option<&std::path::Path>,
    env_mode: Option<&str>,
) -> (Arc<Rules>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut rules = match cfg.allow_toml() {
        Some(t) => Rules::from_toml(&t).unwrap_or_else(|e| {
            warnings.push(format!("config.toml [[allow]] ignored: {e}"));
            Rules::default()
        }),
        None => Rules::default(),
    };
    let (mode, mode_warning) = cfg.permissions.resolve(env_mode);
    warnings.extend(mode_warning);
    if hotl_tools::rules::enforced_build() && mode == hotl_tools::rules::PermissionMode::Auto {
        warnings.push(
            "permissions.mode=auto requested, but this is a security-enforced build — \
             per-action asks stay on"
                .into(),
        );
    }
    rules = rules.with_mode(mode); // enforced builds coerce Auto→Ask inside
    if let Some(path) = admin_path {
        match load_admin(path) {
            Ok(Some(admin)) => rules.merge_admin(admin),
            Ok(None) => {}
            Err(why) => warnings.push(format!(
                "preapproved rules at {} refused: {why}",
                path.display()
            )),
        }
    }
    (Arc::new(rules), warnings)
}

/// Read + trust-check the admin file. `Ok(None)` = file absent (normal).
pub(crate) fn load_admin(
    path: &std::path::Path,
) -> Result<Option<hotl_tools::rules::AdminRules>, String> {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(None);
    };
    hotl_tools::rules::admin_file_trusted(meta.uid(), meta.mode())?;
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    hotl_tools::rules::AdminRules::from_toml(&text)
        .map(Some)
        .map_err(|e| e.to_string())
}

struct Surface {
    handle: SessionHandle,
    json: bool,
    turn_running: bool,
    saw_text: bool,
    /// One SIGINT stream for the surface's lifetime — registered once, not
    /// per select iteration.
    sigint: tokio::signal::unix::Signal,
}

impl Surface {
    fn new(handle: SessionHandle, json: bool) -> Self {
        Self {
            handle,
            json,
            turn_running: false,
            saw_text: false,
            sigint: signal(SignalKind::interrupt()).expect("SIGINT handler"),
        }
    }

    /// Headless: drain events until the (single) turn completes.
    async fn run_until_idle(&mut self) -> i32 {
        self.turn_running = true;
        loop {
            tokio::select! {
                maybe_event = self.handle.events.recv() => {
                    let Some(event) = maybe_event else { return 1 };
                    let done_code = if let EngineEvent::TurnDone { ref outcome, .. } = event {
                        Some(exit_code(outcome))
                    } else {
                        None
                    };
                    self.render(event).await;
                    if let Some(code) = done_code {
                        return code;
                    }
                }
                _ = self.sigint.recv() => self.handle.interrupt(),
            }
        }
    }

    async fn render(&mut self, event: EngineEvent) {
        if self.json {
            self.render_json(event);
            return;
        }
        match event {
            EngineEvent::TextDelta(t) => {
                self.saw_text = true;
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            EngineEvent::ThinkingDelta(_) => {}
            EngineEvent::ToolStart { summary, .. } => {
                if self.saw_text {
                    println!();
                    self.saw_text = false;
                }
                eprintln!("· {summary}");
            }
            EngineEvent::ToolDone { ok, .. } => {
                if !ok {
                    eprintln!("  (tool error — fed back to the model)");
                }
            }
            EngineEvent::ToolDenied { .. } => eprintln!("  (denied)"),
            EngineEvent::ToolAutoAllowed { name, rule } => {
                eprintln!("  (auto-allowed {name} by rule: {rule})");
            }
            EngineEvent::Retrying { attempt, reason } => {
                eprintln!("· retrying ({attempt}): {reason}")
            }
            EngineEvent::FallbackModel { model } => eprintln!("· falling back to {model}"),
            EngineEvent::PromptQueued => eprintln!("(queued — runs after the current turn)"),
            EngineEvent::Compacted { degraded } => {
                if degraded {
                    eprintln!("(context compacted — summary failed, earlier history dropped)");
                } else {
                    eprintln!("(context compacted — earlier history summarized)");
                }
            }
            EngineEvent::Ask { summary, reply, .. } => {
                // Headless asks default-deny; the record goes to stderr.
                eprintln!("hotl: denied (headless): {summary}");
                let _ = reply.send(hotl_engine::AskReply::Deny { message: None });
            }
            EngineEvent::TurnDone { outcome, usage } => self.render_turn_done(outcome, usage),
            EngineEvent::TodosChanged { items } => {
                let done = items
                    .iter()
                    .filter(|t| t.status == hotl_types::TodoStatus::Completed)
                    .count();
                eprintln!("· todos: {done}/{} done", items.len());
            }
        }
    }

    fn render_turn_done(&mut self, outcome: Outcome, usage: hotl_types::TokenUsage) {
        self.turn_running = false;
        match &outcome {
            Outcome::Done { .. } => {}
            Outcome::Cancelled => eprintln!("\n(interrupted)"),
            Outcome::TurnLimit => {
                eprintln!("\nhotl: stopped at max_turns — break the task into smaller prompts.")
            }
            Outcome::Refused => eprintln!("\nhotl: the model declined this request."),
            Outcome::DoomLoop { pattern } => {
                eprintln!("\nhotl: stopped — the model kept repeating: {pattern}")
            }
            Outcome::ToolFailureBudget { tool } => {
                eprintln!("\nhotl: stopped — `{tool}` failed too many times in a row.")
            }
            Outcome::Error { message } => eprintln!("\nhotl: {message}"),
        }
        eprintln!(
            "[in {} out {} cache-read {}]",
            usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens
        );
    }

    fn render_json(&mut self, event: EngineEvent) {
        let v = match event {
            EngineEvent::TextDelta(t) => serde_json::json!({"type":"text_delta","text":t}),
            EngineEvent::ThinkingDelta(_) => serde_json::json!({"type":"thinking_delta"}),
            EngineEvent::ToolStart { name, summary } => {
                serde_json::json!({"type":"tool_start","name":name,"summary":summary})
            }
            EngineEvent::ToolDone { name, ok } => {
                serde_json::json!({"type":"tool_done","name":name,"ok":ok})
            }
            EngineEvent::ToolDenied { name } => {
                serde_json::json!({"type":"tool_denied","name":name})
            }
            EngineEvent::ToolAutoAllowed { name, rule } => {
                serde_json::json!({"type":"tool_auto_allowed","name":name,"rule":rule})
            }
            EngineEvent::Retrying { attempt, reason } => {
                serde_json::json!({"type":"retrying","attempt":attempt,"reason":reason})
            }
            EngineEvent::FallbackModel { model } => {
                serde_json::json!({"type":"fallback_model","model":model})
            }
            EngineEvent::PromptQueued => serde_json::json!({"type":"prompt_queued"}),
            EngineEvent::Compacted { degraded } => {
                serde_json::json!({"type":"compacted","degraded":degraded})
            }
            EngineEvent::Ask { summary, reply, .. } => {
                // JSON mode is headless automation: default-deny, emit the record.
                let _ = reply.send(hotl_engine::AskReply::Deny { message: None });
                serde_json::json!({"type":"ask_denied","summary":summary})
            }
            EngineEvent::TurnDone { outcome, usage } => {
                self.turn_running = false;
                serde_json::json!({"type":"turn_done","outcome":format!("{outcome:?}"),"usage":usage})
            }
            EngineEvent::TodosChanged { items } => {
                serde_json::json!({"type":"todos_changed","items":items})
            }
        };
        // MD contract freeze: every -p/--json frame carries the stable stream
        // schema version so a consumer can pin to it (Tier-1 contract).
        let mut framed = v;
        framed["schema_version"] = serde_json::json!(JSON_STREAM_SCHEMA_VERSION);
        println!("{framed}");
    }
}

/// `(-p prompt, --json)`; `Err(exit_code)` on bad usage.
struct Args {
    prompt: Option<String>,
    json_events: bool,
    schema: Option<PathBuf>,
    name: Option<String>,
}

fn parse_args(args: Vec<String>) -> Result<Args, i32> {
    let mut prompt: Option<String> = None;
    let mut json_events = false;
    let mut schema: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" | "--print" => prompt = iter.next(),
            "--json" => json_events = true,
            "--json-schema" => schema = iter.next().map(PathBuf::from),
            "-n" | "--name" => {
                match iter
                    .next()
                    .as_deref()
                    .and_then(hotl_types::normalize_session_name)
                {
                    Some(n) => name = Some(n),
                    None => {
                        eprintln!("hotl: -n/--name needs a value of 1–64 chars");
                        return Err(2);
                    }
                }
            }
            other => {
                eprintln!("hotl: unknown argument `{other}` (try --help)");
                return Err(2);
            }
        }
    }
    if prompt.is_some() && prompt.as_deref().map(str::trim).unwrap_or("").is_empty() {
        eprintln!("hotl: -p requires a prompt");
        return Err(2);
    }
    if schema.is_some() && prompt.is_none() {
        eprintln!("hotl: --json-schema requires -p \"<prompt>\"");
        return Err(2);
    }
    Ok(Args {
        prompt,
        json_events,
        schema,
        name,
    })
}

/// Secrets-at-rest audit (M2): warn about earlier logs holding values that
/// are secrets *now* — append-only logs can't be scrubbed; the remedy is
/// rotation. Runs off-thread; the current session is masked and excluded.
fn spawn_secret_audit(current_log: PathBuf) {
    std::thread::spawn(move || {
        let masker = Masker::from_env();
        let hits: Vec<_> = hotl_store::audit_secrets(&sessions_dir(), &masker)
            .into_iter()
            .filter(|p| *p != current_log)
            .collect();
        if !hits.is_empty() {
            eprintln!(
                "hotl: WARNING — {} earlier session log(s) contain values that are now \
                 secrets (written before masking could apply). Rotate those secrets. First: {}",
                hits.len(),
                hits[0].display()
            );
        }
    });
}

/// Session-start context: user memory (M2), then project instructions.
fn initial_items(config_dir: &std::path::Path, cwd: &std::path::Path) -> Vec<hotl_types::Item> {
    let mut items = Vec::new();
    if let Some(memory) = load_memory(config_dir) {
        items.push(memory);
    }
    if let Some(instructions) = project_instructions(cwd) {
        items.push(instructions);
    }
    items
}

/// Engine knobs from the environment: HOTL_CONTEXT_WINDOW (tokens) and
/// HOTL_FAST_MODEL (housekeeping model for compaction summaries).
/// Build the engine config from `config.toml [context]` with env overrides
/// (env > config.toml > default).
fn engine_config(
    model: &str,
    secrets: &dyn SecretStore,
    cfg: &crate::config::Config,
) -> EngineConfig {
    let mut config = EngineConfig {
        model: model.to_string(),
        ..Default::default()
    };
    if let Some(window) = secrets
        .get("HOTL_CONTEXT_WINDOW")
        .and_then(|v| v.parse().ok())
        .or(cfg.context.window)
    {
        config.context_window = window;
    }
    config.fast_model = secrets
        .get("HOTL_FAST_MODEL")
        .or_else(|| cfg.provider.fast_model.clone());
    if let Some(t) = secrets
        .get("HOTL_EVICT_TOKENS")
        .and_then(|v| v.parse().ok())
        .or(cfg.context.evict_tokens)
    {
        config.evict_threshold_tokens = t;
    }
    config.compaction_reset = match secrets.get("HOTL_COMPACTION_RESET").as_deref() {
        Some(v) => v == "1",
        None => cfg.context.compaction_reset.unwrap_or(false),
    };
    config.show_context_pct = match secrets.get("HOTL_HIDE_CONTEXT_PCT").as_deref() {
        Some(v) => v != "1",
        None => cfg.context.show_used_pct.unwrap_or(true),
    };
    config
}

fn exit_code(outcome: &Outcome) -> i32 {
    match outcome {
        Outcome::Done { .. } => 0,
        Outcome::Cancelled => 130,
        _ => 1,
    }
}

/// Helper-wins precedence: a configured api-key-helper (env > config.toml)
/// beats static key env vars. `fallback_key` is the provider's static env key.
fn key_source_for(
    cfg: &crate::config::Config,
    secrets: &dyn SecretStore,
    fallback_key: Option<String>,
) -> Arc<dyn hotl_provider::key::KeySource> {
    let cmd = secrets
        .get("HOTL_API_KEY_HELPER")
        .or_else(|| cfg.provider.api_key_helper.clone())
        .filter(|c| !c.trim().is_empty());
    match cmd {
        Some(cmd) => {
            let ttl = secrets
                .get("HOTL_API_KEY_HELPER_TTL_SECS")
                .and_then(|s| s.parse::<u64>().ok())
                .or(cfg.provider.api_key_helper_ttl_secs)
                .map(std::time::Duration::from_secs);
            Arc::new(crate::keysource::HelperKey::new(cmd, ttl))
        }
        None => Arc::new(hotl_provider::key::StaticKey(fallback_key)),
    }
}

type ProviderAndSource = (
    Arc<dyn hotl_provider::Provider>,
    Arc<dyn hotl_provider::key::KeySource>,
);
type SelectedProvider = (
    Arc<dyn hotl_provider::Provider>,
    String,
    Arc<dyn hotl_provider::key::KeySource>,
);

/// Provider/model selection. `HOTL_MODEL` accepts `provider/model`:
///   anthropic/claude-…   needs ANTHROPIC_API_KEY (or [provider] api_key_helper)
///   openai/gpt-…         needs OPENAI_API_KEY (or api_key_helper), or
///                        HOTL_OPENAI_BASE_URL for keyless OpenAI-compatible
///                        endpoints (Ollama etc.)
/// A bare model string means Anthropic; unset means the Anthropic default.
/// Returns the provider, the selected model, and the key source that backs
/// it (so a caller can validate/refresh it once at startup).
pub(crate) fn select_provider(
    cfg: &crate::config::Config,
    secrets: &dyn SecretStore,
) -> Result<SelectedProvider, String> {
    // Precedence: env HOTL_MODEL > config.toml [provider].model > default.
    let (provider_name, model) = selected_model(cfg, secrets);
    let auth = auth_mode(cfg, secrets)?;
    let (provider, source) = match provider_name.as_str() {
        "anthropic" => resolve_anthropic(cfg, secrets, auth)?,
        "openai" | "oai" => resolve_openai(cfg, secrets, auth)?,
        other => {
            return Err(format!(
                "unknown provider `{other}` in HOTL_MODEL. Supported: anthropic/<model>, \
                 openai/<model> (openai covers any OpenAI-compatible endpoint via \
                 HOTL_OPENAI_BASE_URL)."
            ))
        }
    };
    Ok((provider, model, source))
}

/// How hotl authenticates to the selected provider. Orthogonal to *which*
/// provider is selected: both spellings read the same for `anthropic/…` and
/// `openai/…`, so the concept never names a vendor's plan or a proxy project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthMode {
    /// hotl holds and transmits a credential. The default; unchanged behavior.
    ApiKey,
    /// hotl holds no credential; the endpoint authenticates upstream on its
    /// own. Requires `base_url`.
    Subscription,
}

pub(crate) fn auth_mode(
    cfg: &crate::config::Config,
    secrets: &dyn SecretStore,
) -> Result<AuthMode, String> {
    let raw = secrets
        .get("HOTL_PROVIDER_AUTH")
        .or_else(|| cfg.provider.auth.clone());
    match raw.as_deref() {
        None | Some("api_key") => Ok(AuthMode::ApiKey),
        Some("subscription") => Ok(AuthMode::Subscription),
        Some(other) => Err(format!(
            "unknown [provider] auth `{other}`. Valid values: \"api_key\" (default — hotl \
             holds the credential) or \"subscription\" (hotl holds no credential; the \
             endpoint authenticates upstream, and base_url is required)."
        )),
    }
}

/// The active endpoint, if one is configured. `HOTL_ANTHROPIC_BASE_URL` is the
/// Anthropic-side twin of the long-standing `HOTL_OPENAI_BASE_URL`.
fn anthropic_base_url(cfg: &crate::config::Config, secrets: &dyn SecretStore) -> Option<String> {
    secrets
        .get("HOTL_ANTHROPIC_BASE_URL")
        .or_else(|| cfg.provider.base_url.clone())
}

/// `(provider_name, model)` from `HOTL_MODEL` / config / default. A bare
/// model string means Anthropic.
fn selected_model(cfg: &crate::config::Config, secrets: &dyn SecretStore) -> (String, String) {
    let raw = secrets
        .get("HOTL_MODEL")
        .or_else(|| cfg.provider.model.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    match raw.split_once('/') {
        Some((p, m)) => (p.to_ascii_lowercase(), m.to_string()),
        None => ("anthropic".to_string(), raw),
    }
}

/// The endpoint the active provider will actually use, when it is not the
/// vendor's own. `None` means a direct connection. `hotl doctor` probes this.
pub(crate) fn active_endpoint(
    cfg: &crate::config::Config,
    secrets: &dyn SecretStore,
) -> Option<String> {
    match selected_model(cfg, secrets).0.as_str() {
        "openai" | "oai" => secrets
            .get("HOTL_OPENAI_BASE_URL")
            .or_else(|| cfg.provider.base_url.clone())
            .filter(|b| b != hotl_provider_openai::DEFAULT_BASE_URL),
        _ => anthropic_base_url(cfg, secrets),
    }
}

fn subscription_needs_base_url(env_var: &str) -> String {
    format!(
        "[provider] auth = \"subscription\" requires base_url — hotl holds no credential in \
         this mode, so it needs an endpoint that authenticates on its own. Set [provider] \
         base_url (or {env_var}) to that endpoint, or use auth = \"api_key\"."
    )
}

/// Warn when traffic crosses the network in the clear. Which exposure matters
/// depends on the mode: under `api_key` a bearer credential is at stake, under
/// `subscription` there is no credential but prompts and session content still
/// travel unencrypted. One predicate, two messages — loopback http is the
/// normal local-endpoint case and is never warned on.
fn warn_cleartext(base: &str, auth: AuthMode, credential_present: bool) {
    if !cleartext_nonloopback(base) {
        return;
    }
    match auth {
        AuthMode::Subscription => eprintln!(
            "hotl: WARNING — [provider] base_url is a non-loopback http:// URL; prompts and \
             session content will cross the network unencrypted. Use https:// or an SSH tunnel."
        ),
        AuthMode::ApiKey if credential_present => eprintln!(
            "hotl: WARNING — [provider] base_url is a non-loopback http:// URL and an API key \
             is set; the key will cross the network unencrypted. Use https:// or an SSH tunnel."
        ),
        AuthMode::ApiKey => {}
    }
}

fn resolve_anthropic(
    cfg: &crate::config::Config,
    secrets: &dyn SecretStore,
    auth: AuthMode,
) -> Result<ProviderAndSource, String> {
    let base = anthropic_base_url(cfg, secrets);
    if auth == AuthMode::Subscription {
        let base = base.ok_or_else(|| subscription_needs_base_url("HOTL_ANTHROPIC_BASE_URL"))?;
        warn_cleartext(&base, auth, false);
        // A keyless source, deliberately: selection refuses to hand the
        // provider a credential, and the provider refuses to consult one.
        // Either half alone would suffice; both means no wiring mistake in
        // one layer can leak an environment key to a bridge.
        let source: Arc<dyn hotl_provider::key::KeySource> =
            Arc::new(hotl_provider::key::StaticKey(None));
        let provider = AnthropicProvider::new(source.clone())
            .with_base_url(&base)
            .subscription();
        return Ok((Arc::new(provider), source));
    }
    let key = secrets.get("ANTHROPIC_API_KEY");
    let source = key_source_for(cfg, secrets, key.clone());
    if !source.refreshable() && key.is_none() {
        return Err(
            "ANTHROPIC_API_KEY is not set and no api_key_helper is configured.\n\
             Export the key, set [provider] api_key_helper in config.toml, point [provider] \
             base_url at an endpoint that authenticates for you and set auth = \
             \"subscription\", or select another provider, e.g. HOTL_MODEL=openai/<model> \
             (with OPENAI_API_KEY, or HOTL_OPENAI_BASE_URL for a local endpoint). \
             `hotl watch` needs no key."
                .to_string(),
        );
    }
    let mut provider = AnthropicProvider::new(source.clone());
    if let Some(base) = &base {
        warn_cleartext(base, auth, key.is_some() || source.refreshable());
        provider = provider.with_base_url(base);
    }
    Ok((Arc::new(provider), source))
}

fn resolve_openai(
    cfg: &crate::config::Config,
    secrets: &dyn SecretStore,
    auth: AuthMode,
) -> Result<ProviderAndSource, String> {
    let configured = secrets
        .get("HOTL_OPENAI_BASE_URL")
        .or_else(|| cfg.provider.base_url.clone());
    if auth == AuthMode::Subscription {
        let base = configured.ok_or_else(|| subscription_needs_base_url("HOTL_OPENAI_BASE_URL"))?;
        warn_cleartext(&base, auth, false);
        let source: Arc<dyn hotl_provider::key::KeySource> =
            Arc::new(hotl_provider::key::StaticKey(None));
        return Ok((
            Arc::new(hotl_provider_openai::OpenAiCompatProvider::new(
                base,
                source.clone(),
            )),
            source,
        ));
    }
    let base = configured.unwrap_or_else(|| hotl_provider_openai::DEFAULT_BASE_URL.to_string());
    let key = secrets.get("OPENAI_API_KEY");
    let source = key_source_for(cfg, secrets, key.clone());
    if !source.refreshable() && key.is_none() && base == hotl_provider_openai::DEFAULT_BASE_URL {
        return Err(
            "OPENAI_API_KEY is not set (required for api.openai.com; keyless works \
                     only with HOTL_OPENAI_BASE_URL pointing at a local/compatible endpoint, \
                     e.g. http://localhost:11434/v1 for Ollama), or configure [provider] \
                     api_key_helper."
                .to_string(),
        );
    }
    // H-09: a bearer key over cleartext http:// to a non-loopback host
    // crosses the network unencrypted. Warn loudly (don't silently send
    // it); loopback http is the normal local-endpoint case. A helper-sourced
    // key (source.refreshable()) is just as real a bearer credential as the
    // static env key, so it must trip this warning too.
    warn_cleartext(&base, auth, key.is_some() || source.refreshable());
    Ok((
        Arc::new(hotl_provider_openai::OpenAiCompatProvider::new(
            base,
            source.clone(),
        )),
        source,
    ))
}

/// A cleartext base URL pointing somewhere other than the local machine.
///
/// Trims and lowercases first, so this is the single normalization point
/// shared by both provider paths. Neither `v1_base` nor the OpenAI provider
/// trims whitespace, and neither cares about scheme case — so a value with a
/// leading space (realistic from a `.env` or a systemd `EnvironmentFile`) or
/// an uppercase `HTTP://` used to skip the warning while the request still
/// went out in the clear.
///
/// Fails closed: anything not recognizably `https://` (always exempt) or
/// `http://` is still handed straight to the HTTP client, so a value we
/// cannot classify warns rather than silently passing as safe.
fn cleartext_nonloopback(base: &str) -> bool {
    let base = base.trim().to_ascii_lowercase();
    if base.is_empty() || base.starts_with("https://") {
        return false;
    }
    let Some(authority) = base.strip_prefix("http://") else {
        return true;
    };
    let host = host_of(authority);
    !matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") && !host.is_empty()
}

/// The host out of a URL authority, keeping a bracketed IPv6 literal intact.
///
/// Splitting on `:` alone truncates `[::1]:3456` to `[`, which is why the
/// IPv6 loopback arms above never matched and a local endpoint drew a
/// network-exposure warning.
fn host_of(authority: &str) -> &str {
    let authority = authority.split('/').next().unwrap_or("");
    if authority.starts_with('[') {
        return match authority.find(']') {
            Some(close) => &authority[..=close],
            None => authority,
        };
    }
    authority.split(':').next().unwrap_or("")
}

pub(crate) fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hotl")
}

/// `<xdg-data>/hotl` — the state/data root (sessions, shadows, history),
/// falling back to `~/.local/share/hotl`.
pub(crate) fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hotl")
}

pub(crate) fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/`-dispatch names come out of the registry's own discovery walk.
    /// If this ever needs a second `SkillTool::new`, the roster is being
    /// scanned twice per start again.
    #[test]
    fn build_registry_yields_the_skill_names_it_discovered() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(skills.join("deploy.md"), "# Deploy checklist\nsteps\n").unwrap();

        // Pin the Claude roots off: they are real directories on the
        // developer's machine and would leak into this assertion.
        let mut cfg = crate::config::Config::default();
        cfg.skills.claude = Some(false);
        let (_registry, names) = build_registry(&cfg, dir.path(), None);
        assert_eq!(names, vec!["deploy".to_string()]);

        // No skills configured → no names, and no tool registered.
        let empty = tempfile::tempdir().unwrap();
        let (_registry, names) = build_registry(&cfg, empty.path(), None);
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    #[cfg(not(feature = "security-enforced"))] // asserts the auto default
    fn load_rules_merges_trusted_admin_file_and_reports_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let admin = dir.path().join("preapproved.toml");
        std::fs::write(&admin, "[[allow]]\ntool = \"bash\"\nprefix = \"git \"\n").unwrap();
        // World-writable → refused with a warning naming the file.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&admin, std::fs::Permissions::from_mode(0o666)).unwrap();
        let (rules, warnings) =
            load_rules_with(&crate::config::Config::default(), Some(&admin), None);
        assert!(
            warnings.iter().any(|w| w.contains("preapproved")),
            "warnings: {warnings:?}"
        );
        // Refused file contributes nothing; mode default auto still applies.
        assert!(matches!(
            rules.evaluate(
                rules.mode(),
                "bash",
                &serde_json::json!({"command": "git status"}),
                true,
                false,
                false
            ),
            hotl_tools::rules::Verdict::Auto { rule } if rule == "permissions.mode=auto"
        ));
        // Absent file: no warning, auto default.
        let (_, warnings) = load_rules_with(
            &crate::config::Config::default(),
            Some(&dir.path().join("nope.toml")),
            None,
        );
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        // Explicit ask via the env seam.
        let (rules, _) = load_rules_with(&crate::config::Config::default(), None, Some("ask"));
        assert_eq!(rules.mode(), hotl_tools::rules::PermissionMode::Ask);
    }

    /// In-memory `SecretStore` for tests — no real env mutation, no races
    /// between tests running in parallel.
    #[derive(Default)]
    struct MapSecrets(std::collections::HashMap<String, String>);

    impl<const N: usize> From<[(&str, &str); N]> for MapSecrets {
        fn from(pairs: [(&str, &str); N]) -> Self {
            MapSecrets(
                pairs
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        }
    }

    impl SecretStore for MapSecrets {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    /// Same construction the `config.rs` tests use: write the TOML to a
    /// tempdir and load it, so `[provider]` parsing goes through the real path.
    fn config_from_toml(toml: &str) -> crate::config::Config {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), toml).unwrap();
        crate::config::Config::load(dir.path())
    }

    #[test]
    fn helper_beats_static_key_env() {
        let cfg = config_from_toml("[provider]\napi_key_helper = \"echo k\"\n");
        let secrets = MapSecrets::from([
            ("OPENAI_API_KEY", "sk-static"),
            ("HOTL_MODEL", "openai/m"),
            ("HOTL_OPENAI_BASE_URL", "http://localhost:1/v1"),
        ]);
        let (_p, _m, source) = select_provider(&cfg, &secrets).unwrap();
        assert!(
            source.refreshable(),
            "helper must win over the static env key"
        );
    }

    #[test]
    fn empty_helper_command_falls_back_to_static_key() {
        let cfg = config_from_toml("[provider]\napi_key_helper = \"\"\n");
        let secrets = MapSecrets::from([
            ("OPENAI_API_KEY", "sk-static"),
            ("HOTL_MODEL", "openai/m"),
            ("HOTL_OPENAI_BASE_URL", "http://localhost:1/v1"),
        ]);
        let (_p, _m, source) = select_provider(&cfg, &secrets).unwrap();
        assert!(
            !source.refreshable(),
            "empty api_key_helper must not activate the helper"
        );
    }

    #[test]
    fn helper_env_var_activates_without_config() {
        let cfg = config_from_toml("");
        let secrets = MapSecrets::from([
            ("HOTL_API_KEY_HELPER", "echo k"),
            ("HOTL_MODEL", "openai/m"),
            ("HOTL_OPENAI_BASE_URL", "http://localhost:1/v1"),
        ]);
        let (_p, _m, source) = select_provider(&cfg, &secrets).unwrap();
        assert!(source.refreshable());
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn subscription_auth_without_base_url_is_refused() {
        // Fail closed: otherwise hotl sends a placeholder credential to the
        // vendor's own endpoint and the user debugs a 401 instead of config.
        let cfg = config_from_toml("[provider]\nauth = \"subscription\"\n");
        let secrets = MapSecrets::from([("HOTL_MODEL", "anthropic/m")]);
        let err = select_provider(&cfg, &secrets).err().unwrap();
        assert!(err.contains("base_url"), "{err}");
    }

    #[test]
    fn subscription_auth_needs_no_key() {
        let cfg = config_from_toml(
            "[provider]\nauth = \"subscription\"\nbase_url = \"http://127.0.0.1:3456\"\n",
        );
        let secrets = MapSecrets::from([("HOTL_MODEL", "anthropic/m")]);
        let (_p, m, _s) = select_provider(&cfg, &secrets).unwrap();
        assert_eq!(m, "m");
    }

    /// Selection-layer half of the credential suppression. The provider
    /// refuses to consult the source; selection refuses to hand it one.
    #[test]
    fn subscription_auth_discards_an_available_key() {
        let cfg = config_from_toml(
            "[provider]\nauth = \"subscription\"\nbase_url = \"http://127.0.0.1:3456\"\n\
             api_key_helper = \"echo leaked\"\n",
        );
        let secrets = MapSecrets::from([
            ("HOTL_MODEL", "anthropic/m"),
            ("ANTHROPIC_API_KEY", "sk-ant-real-secret"),
        ]);
        let (_p, _m, source) = select_provider(&cfg, &secrets).unwrap();
        assert!(
            !source.refreshable(),
            "subscription mode must not carry a refreshable key source"
        );
        assert_eq!(
            block_on(source.get()).unwrap(),
            None,
            "subscription mode must not carry a key"
        );
    }

    #[test]
    fn subscription_auth_works_for_openai_too() {
        let cfg = config_from_toml(
            "[provider]\nauth = \"subscription\"\nbase_url = \"http://127.0.0.1:4000/v1\"\n",
        );
        let secrets = MapSecrets::from([("HOTL_MODEL", "openai/m")]);
        let (_p, _m, source) = select_provider(&cfg, &secrets).unwrap();
        assert_eq!(block_on(source.get()).unwrap(), None);
    }

    #[test]
    fn unknown_auth_mode_names_the_valid_values() {
        let cfg = config_from_toml("[provider]\nauth = \"oauth\"\n");
        let secrets = MapSecrets::from([("HOTL_MODEL", "anthropic/m")]);
        let err = select_provider(&cfg, &secrets).err().unwrap();
        assert!(
            err.contains("api_key") && err.contains("subscription"),
            "{err}"
        );
    }

    #[test]
    fn anthropic_base_url_env_overrides_config() {
        let cfg = config_from_toml(
            "[provider]\nauth = \"subscription\"\nbase_url = \"http://127.0.0.1:1/v1\"\n",
        );
        let secrets = MapSecrets::from([
            ("HOTL_MODEL", "anthropic/m"),
            ("HOTL_ANTHROPIC_BASE_URL", "http://127.0.0.1:9999"),
        ]);
        // Selection must succeed; the env value is what the provider gets.
        assert!(select_provider(&cfg, &secrets).is_ok());
    }

    /// The predicate behind every cleartext warning. `https://` is always
    /// exempt; loopback is exempt because nothing leaves the machine.
    #[test]
    fn cleartext_exempts_https_and_loopback() {
        for safe in [
            "https://gateway.example",
            "https://gateway.example/v1",
            "HTTPS://gateway.example",
            "http://localhost:3456",
            "http://127.0.0.1:3456/v1",
            "http://[::1]:3456",
        ] {
            assert!(!cleartext_nonloopback(safe), "should not warn: {safe}");
        }
    }

    /// Anything hotl cannot classify still gets handed to the HTTP client,
    /// so "can't tell" must warn rather than pass as safe. Untrimmed input
    /// is realistic from a `.env` file or a systemd `EnvironmentFile`, and
    /// an uppercase scheme is a URL the client accepts and we did not.
    #[test]
    fn cleartext_fails_closed_on_unclassifiable_input() {
        for risky in [
            "http://gateway.example",
            " http://gateway.example",
            "\thttp://gateway.example\n",
            "HTTP://gateway.example",
            "gateway.example:8080",
            "ftp://gateway.example",
        ] {
            assert!(cleartext_nonloopback(risky), "should warn: {risky}");
        }
    }

    /// Trimming must not turn a loopback URL into a warning.
    #[test]
    fn cleartext_trims_before_classifying_loopback() {
        assert!(!cleartext_nonloopback("  http://127.0.0.1:3456  "));
    }

    /// api_key mode must keep working exactly as before, including the
    /// OpenAI provider's existing keyless-on-custom-base allowance.
    #[test]
    fn api_key_mode_preserves_openai_keyless_custom_base() {
        let cfg = config_from_toml("");
        let secrets = MapSecrets::from([
            ("HOTL_MODEL", "openai/m"),
            ("HOTL_OPENAI_BASE_URL", "http://localhost:11434/v1"),
        ]);
        assert!(select_provider(&cfg, &secrets).is_ok());
    }

    #[test]
    fn keyless_openai_default_base_error_mentions_helper() {
        let cfg = config_from_toml("");
        let secrets = MapSecrets::from([("HOTL_MODEL", "openai/m")]);
        // `Arc<dyn Provider>` isn't `Debug`, so `unwrap_err()` (which needs
        // the Ok side to be `Debug` for its panic message) doesn't apply.
        let err = select_provider(&cfg, &secrets).err().unwrap();
        assert!(err.contains("api_key_helper"), "{err}");
    }

    #[test]
    fn anthropic_without_key_or_helper_errors_with_instruction() {
        let cfg = config_from_toml("");
        let err = select_provider(&cfg, &MapSecrets::default()).err().unwrap();
        assert!(err.contains("ANTHROPIC_API_KEY"), "{err}");
        assert!(err.contains("api_key_helper"), "{err}");
    }

    #[test]
    fn parse_args_accepts_name() {
        let args: Vec<String> = ["-p", "hi", "-n", "  fix-auth  "]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parsed = parse_args(args).expect("parses");
        assert_eq!(parsed.name.as_deref(), Some("fix-auth"));
    }

    #[test]
    fn parse_args_rejects_bad_names() {
        for bad in [vec!["-p", "hi", "-n"], vec!["-p", "hi", "-n", "   "]] {
            let args: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert!(parse_args(args).is_err());
        }
    }
}
