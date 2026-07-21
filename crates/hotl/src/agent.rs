//! The execute surface: a steering REPL and `-p` headless.
//!
//! The surface is a client of the session actor: it renders events, answers
//! asks, and turns typed lines into prompts (idle) or steers (mid-turn).

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use hotl_context::{load_memory, load_system_prompt, project_instructions};
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::{Clock, EnvSecrets, SecretStore, SystemClock};
use hotl_provider_anthropic::{AnthropicProvider, DEFAULT_MODEL};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, sandbox, Registry};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

const ASK_TIMEOUT_SECS: u64 = 300;

/// Stable schema version of the `-p --json` event stream (MD Tier-1 contract;
/// bump only on a breaking change to a frame's shape).
pub const JSON_STREAM_SCHEMA_VERSION: u32 = 1;

/// Context inherited from an earlier session (`hotl resume` — M3b).
pub(crate) struct Resumed {
    pub parent_id: String,
    pub items: Vec<hotl_types::Item>,
}

pub async fn agent_main(args: Vec<String>) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    match (parsed.schema, parsed.prompt) {
        (Some(schema), Some(prompt)) => structured_main(&prompt, &schema).await,
        (_, prompt) => run_session(prompt, parsed.json_events, None).await,
    }
}

/// `hotl -p "…" --json-schema <file>` (T2): run one headless turn, validate the
/// answer against the schema (with bounded retry), print the JSON or exit 1.
async fn structured_main(prompt: &str, schema_path: &std::path::Path) -> i32 {
    let schema: serde_json::Value = match std::fs::read_to_string(schema_path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hotl: could not read --json-schema `{}`: {e}", schema_path.display());
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
    let log = match SessionLog::create(&sessions_dir(), &scaffold.model, None, scaffold.masker(), scaffold.clock.now_ms()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl: could not create session log: {e}");
            return 1;
        }
    };
    let mut items = initial_items(&scaffold.config_dir, &scaffold.cwd);
    items.push(crate::structured::contract_item(&schema));
    let mut handle = spawn_session(scaffold.deps(log, None, items));
    match crate::structured::run_structured(&mut handle, &schema, prompt, crate::structured::MAX_RETRIES).await {
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

/// `hotl resume [id-prefix]`: bare lists recent sessions; with a prefix,
/// replays that session's lineage into a fresh session's context.
pub async fn resume_main(args: Vec<String>) -> i32 {
    let dir = sessions_dir();
    let sessions = hotl_store::list_sessions(&dir);
    let Some(prefix) = args.first() else {
        if sessions.is_empty() {
            println!("no sessions yet — run `hotl` first");
        } else {
            println!("recent sessions (resume with `hotl resume <id-prefix>`):");
            for (id, _, modified) in sessions.iter().take(10) {
                println!("  {id}  ({})", age(*modified));
            }
        }
        return 0;
    };
    let Some((id, _, _)) = sessions.iter().find(|(id, ..)| id.starts_with(prefix.as_str())) else {
        eprintln!("hotl: no session starts with `{prefix}` (try bare `hotl resume` to list)");
        return 1;
    };
    match hotl_store::replay_chain(&dir, id) {
        Ok(replayed) => {
            for warning in &replayed.warnings {
                eprintln!("hotl: WARNING — {warning}");
            }
            let resumed = Resumed { parent_id: replayed.header.session_id, items: replayed.items };
            run_session(None, false, Some(resumed)).await
        }
        Err(e) => {
            eprintln!("hotl: could not replay session: {e}");
            1
        }
    }
}

/// `hotl acp`: serve the ACP JSON-RPC protocol over stdio (M4). Wires the
/// real engine deps into a session factory and hands the streams to the
/// protocol loop. One connection, one process (process-per-session).
pub async fn acp_main() -> i32 {
    let (factory, _model) = match acp_factory().await {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    crate::acp::serve(tokio::io::stdin(), tokio::io::stdout(), factory).await;
    0
}

/// The real-engine session factory `hotl acp` and `hotl tui` share, plus the
/// resolved model name. Prints its own errors; `Err` carries the exit code.
pub(crate) async fn acp_factory() -> Result<(crate::acp::SessionFactory, String), i32> {
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
    let factory: crate::acp::SessionFactory = Box::new(move |spec| {
        let resumed = match spec {
            crate::acp::SessionSpec::New => None,
            crate::acp::SessionSpec::Load(sid) => {
                let replayed = hotl_store::replay_chain(&sessions_dir(), &sid)
                    .map_err(|e| format!("could not load session {sid}: {e}"))?;
                Some(Resumed { parent_id: replayed.header.session_id, items: replayed.items })
            }
        };
        let parent_id = resumed.as_ref().map(|r| r.parent_id.clone());
        let log = SessionLog::create(&sessions_dir(), &scaffold.model, parent_id, scaffold.masker(), scaffold.clock.now_ms())
            .map_err(|e| format!("could not create session log: {e}"))?;
        let session_id = log.session_id.clone();
        let (snapshots, initial) = session_context(&session_id, &scaffold.cwd, &scaffold.config_dir, &resumed);
        Ok(spawn_session(scaffold.deps(log, snapshots, initial)))
    });
    Ok((factory, model))
}

/// `hotl serve --id <id> [--prompt <p>]`: build a session and host it on a
/// unix socket for `hotl attach` (the detached-session server behind `hotl bg`).
pub async fn serve_main(id: String, prompt: Option<String>) -> i32 {
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
    let log = match SessionLog::create(&sessions_dir(), &scaffold.model, None, scaffold.masker(), scaffold.clock.now_ms()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl serve: could not create session log: {e}");
            return 1;
        }
    };
    let session_id = log.session_id.clone();
    let (snapshots, initial_items) = session_context(&session_id, &scaffold.cwd, &scaffold.config_dir, &None);
    let handle = spawn_session(scaffold.deps(log, snapshots, initial_items));
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
    sandbox_status: sandbox::SandboxStatus,
    cwd: PathBuf,
    config: EngineConfig,
    registry: Arc<Registry>,
    hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
    /// The parsed config.toml, loaded once per process and shared with every
    /// helper that used to re-read the file.
    cfg: crate::config::Config,
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
        provider.clone(), rules.clone(), clock.clone(), config.clone(),
        cwd.clone(), cfg.hooks_toml(), system.clone(), model.clone(), sandbox_enforced,
        initial_helper_key.clone(),
    );
    let registry = Arc::new(build_registry(&cfg, &config_dir, Some(spawn_builder)));
    let hooks = load_hooks(&cfg);
    Ok(Scaffold {
        provider, model, clock, config_dir, system, rules, sandbox_enforced,
        sandbox_status, cwd, config, registry, hooks, cfg, initial_helper_key,
    })
}

impl Scaffold {
    /// Session masker: env-named secrets plus the helper-acquired key.
    /// Refreshed keys are NOT re-registered: keys never enter log entries;
    /// this registration is defense-in-depth for the startup key.
    pub(crate) fn masker(&self) -> Masker {
        masker_with_helper(self.initial_helper_key.as_deref())
    }

    fn deps(
        &self,
        log: SessionLog,
        snapshots: Option<Arc<dyn hotl_engine::Snapshotter>>,
        initial_items: Vec<hotl_types::Item>,
    ) -> SessionDeps {
        SessionDeps {
            provider: self.provider.clone(),
            registry: self.registry.clone(),
            rules: self.rules.clone(),
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

fn age(t: std::time::SystemTime) -> String {
    let secs = t.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

async fn run_session(prompt: Option<String>, json_events: bool, resumed: Option<Resumed>) -> i32 {
    let headless = prompt.is_some();
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

    let parent_id = resumed.as_ref().map(|r| r.parent_id.clone());
    let log = match SessionLog::create(&sessions_dir(), &scaffold.model, parent_id, scaffold.masker(), scaffold.clock.now_ms()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl: could not create session log: {e}");
            return 1;
        }
    };
    let session_id = log.session_id.clone();
    spawn_secret_audit(log.path().to_path_buf());
    let gc_config_dir = scaffold.config_dir.clone();
    std::thread::spawn(move || crate::gc::auto_gc(&gc_config_dir)); // retention, off the hot path
    let (snapshots, initial_items) = session_context(&session_id, &scaffold.cwd, &scaffold.config_dir, &resumed);
    let handle = spawn_session(scaffold.deps(log, snapshots, initial_items));

    let mut surface =
        Surface::new(handle, headless, json_events, ask_timeout_from_env(&secrets, &scaffold.cfg));
    if let Some(p) = prompt {
        surface.handle.prompt(crate::setup::expand_file_refs(&p)).await;
        return surface.run_until_idle().await;
    }
    if let Some(r) = &resumed {
        println!("resumed from session {} ({} items of context)", r.parent_id, r.items.len());
        // #8: continue an interrupted turn (last item is the model's to answer).
        if hotl_engine::needs_continuation(&r.items) {
            println!("(continuing the interrupted turn…)");
            surface.handle.continue_turn().await;
            surface.turn_running = true;
        }
    }
    if let Some(hint) = crate::setup::first_run_hint(&scaffold.config_dir) {
        eprintln!("hotl: {hint}");
    }
    print_banner(&scaffold.model, &session_id, &scaffold.sandbox_status);
    surface.repl().await
}

/// Builtins + the `mcp` meta-tool (M3a) + the `spawn` tool (M4) when a child
/// builder is supplied. `spawn` is omitted for child sessions, so sub-agents
/// cannot recurse (structural depth cap).
fn build_registry(
    cfg: &crate::config::Config,
    config_dir: &std::path::Path,
    spawn_builder: Option<Arc<dyn crate::spawn::ChildBuilder>>,
) -> Registry {
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
    if hotl_tools::skills::SkillTool::has_skills(config_dir) {
        registry.register(Box::new(hotl_tools::skills::SkillTool::new(config_dir)));
    }
    if let Some(builder) = spawn_builder {
        registry.register(Box::new(crate::spawn::SpawnTool::new(builder)));
    }
    registry
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
        let log = SessionLog::create(&sessions_dir(), &self.model, None, self.masker(), self.clock.now_ms())
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
) -> (Option<Arc<dyn hotl_engine::Snapshotter>>, Vec<hotl_types::Item>) {
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
    sessions_dir().parent().map(|p| p.join("shadow")).unwrap_or_else(|| PathBuf::from("shadow"))
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

/// Allow-rules from config.toml `[[allow]]`.
fn load_rules(cfg: &crate::config::Config) -> Arc<Rules> {
    let rules = match cfg.allow_toml() {
        Some(t) => Rules::from_toml(&t).unwrap_or_else(|e| {
            eprintln!("hotl: config.toml [[allow]] ignored: {e}");
            Rules::default()
        }),
        None => Rules::default(),
    };
    Arc::new(rules)
}

fn print_banner(model: &str, session_id: &str, status: &sandbox::SandboxStatus) {
    println!(
        "hotl · {model} · session {session_id} · {}",
        match status {
            sandbox::SandboxStatus::Enforced(m) => format!("sandbox:{m}"),
            other => other.label(),
        }
    );
    println!("type to prompt · type mid-turn to steer · ctrl-c interrupts · ctrl-d exits");
}

struct Surface {
    handle: SessionHandle,
    headless: bool,
    json: bool,
    stdin: mpsc::Receiver<String>,
    turn_running: bool,
    saw_text: bool,
    /// How long an interactive permission ask waits before default-denying.
    /// `None` = wait indefinitely (a backgrounded/detached session holds the
    /// ask until you reattach and answer — `HOTL_ASK_TIMEOUT=0`).
    ask_timeout: Option<std::time::Duration>,
    /// One SIGINT stream for the surface's lifetime — registered once, not
    /// per select iteration, and shared with `ask_human` so Ctrl-C during a
    /// permission ask isn't dropped.
    sigint: tokio::signal::unix::Signal,
}

impl Surface {
    fn new(
        handle: SessionHandle,
        headless: bool,
        json: bool,
        ask_timeout: Option<std::time::Duration>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(8);
        if !headless {
            std::thread::spawn(move || {
                let stdin = std::io::stdin();
                let mut line = String::new();
                loop {
                    line.clear();
                    match stdin.read_line(&mut line) {
                        Ok(0) | Err(_) => break, // EOF
                        Ok(_) => {
                            if tx.blocking_send(line.clone()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
        Self {
            handle,
            headless,
            json,
            stdin: rx,
            turn_running: false,
            saw_text: false,
            ask_timeout,
            sigint: signal(SignalKind::interrupt()).expect("SIGINT handler"),
        }
    }

    /// Interactive loop. Lines while idle → prompts; lines mid-turn → steers.
    async fn repl(&mut self) -> i32 {
        self.prompt_marker();
        loop {
            tokio::select! {
                maybe_event = self.handle.events.recv() => {
                    let Some(event) = maybe_event else { return 0 };
                    self.render(event).await;
                }
                maybe_line = self.stdin.recv() => {
                    let Some(line) = maybe_line else { println!(); return 0 };
                    let line = line.trim().to_string();
                    if line.is_empty() { if !self.turn_running { self.prompt_marker(); } continue; }
                    if !self.turn_running && matches!(line.as_str(), "exit" | "quit") { return 0; }
                    if self.turn_running {
                        self.handle.steer(crate::setup::expand_file_refs(&line)).await;
                        eprintln!("(steered — woven into the agent's next step)");
                    } else {
                        self.handle.prompt(crate::setup::expand_file_refs(&line)).await;
                        self.turn_running = true;
                    }
                }
                _ = self.sigint.recv() => {
                    if self.turn_running {
                        self.handle.interrupt();
                    } else {
                        println!();
                        return 0;
                    }
                }
            }
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

    fn prompt_marker(&self) {
        if !self.headless {
            eprint!("\n❯ ");
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
            EngineEvent::Retrying { attempt, reason } => eprintln!("· retrying ({attempt}): {reason}"),
            EngineEvent::FallbackModel { model } => eprintln!("· falling back to {model}"),
            EngineEvent::PromptQueued => eprintln!("(queued — runs after the current turn)"),
            EngineEvent::Compacted { degraded } => {
                if degraded {
                    eprintln!("(context compacted — summary failed, earlier history dropped)");
                } else {
                    eprintln!("(context compacted — earlier history summarized)");
                }
            }
            EngineEvent::Ask { summary, protected_why, reply } => {
                let answer = self.ask_human(&summary, protected_why.as_deref()).await;
                let _ = reply.send(answer);
            }
            EngineEvent::TurnDone { outcome, usage } => self.render_turn_done(outcome, usage),
        }
    }

    fn render_turn_done(&mut self, outcome: Outcome, usage: hotl_types::TokenUsage) {
        self.turn_running = false;
        match &outcome {
            Outcome::Done { .. } => {}
            Outcome::Cancelled => eprintln!("\n(interrupted)"),
            Outcome::TurnLimit => eprintln!("\nhotl: stopped at max_turns — break the task into smaller prompts."),
            Outcome::Refused => eprintln!("\nhotl: the model declined this request."),
            Outcome::DoomLoop { pattern } => eprintln!("\nhotl: stopped — the model kept repeating: {pattern}"),
            Outcome::ToolFailureBudget { tool } => {
                eprintln!("\nhotl: stopped — `{tool}` failed too many times in a row.")
            }
            Outcome::Error { message } => eprintln!("\nhotl: {message}"),
        }
        eprintln!(
            "[in {} out {} cache-read {}]",
            usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens
        );
        self.prompt_marker();
    }

    fn render_json(&mut self, event: EngineEvent) {
        let v = match event {
            EngineEvent::TextDelta(t) => serde_json::json!({"type":"text_delta","text":t}),
            EngineEvent::ThinkingDelta(_) => serde_json::json!({"type":"thinking_delta"}),
            EngineEvent::ToolStart { name, summary } => serde_json::json!({"type":"tool_start","name":name,"summary":summary}),
            EngineEvent::ToolDone { name, ok } => serde_json::json!({"type":"tool_done","name":name,"ok":ok}),
            EngineEvent::ToolDenied { name } => serde_json::json!({"type":"tool_denied","name":name}),
            EngineEvent::ToolAutoAllowed { name, rule } => serde_json::json!({"type":"tool_auto_allowed","name":name,"rule":rule}),
            EngineEvent::Retrying { attempt, reason } => serde_json::json!({"type":"retrying","attempt":attempt,"reason":reason}),
            EngineEvent::FallbackModel { model } => serde_json::json!({"type":"fallback_model","model":model}),
            EngineEvent::PromptQueued => serde_json::json!({"type":"prompt_queued"}),
            EngineEvent::Compacted { degraded } => serde_json::json!({"type":"compacted","degraded":degraded}),
            EngineEvent::Ask { summary, reply, .. } => {
                // JSON mode is headless automation: default-deny, emit the record.
                let _ = reply.send(hotl_engine::AskReply::Deny { message: None });
                serde_json::json!({"type":"ask_denied","summary":summary})
            }
            EngineEvent::TurnDone { outcome, usage } => {
                self.turn_running = false;
                serde_json::json!({"type":"turn_done","outcome":format!("{outcome:?}"),"usage":usage})
            }
        };
        // MD contract freeze: every -p/--json frame carries the stable stream
        // schema version so a consumer can pin to it (Tier-1 contract).
        let mut framed = v;
        framed["schema_version"] = serde_json::json!(JSON_STREAM_SCHEMA_VERSION);
        println!("{framed}");
    }

    async fn ask_human(&mut self, summary: &str, protected_why: Option<&str>) -> hotl_engine::AskReply {
        use hotl_engine::AskReply;
        if self.headless || !std::io::stdin().is_terminal() {
            eprintln!("hotl: denied (headless): {summary}");
            return AskReply::Deny { message: None };
        }
        if let Some(why) = protected_why {
            eprintln!("⚠ PROTECTED PATH — {why}");
        }
        // A bare `n` denies; `n <reason>` sends the reason to the model (T1).
        eprint!("allow {summary}? [y/N — add a reason after 'n' to tell the model why] ");
        // Ctrl-C while the ask is parked = deny + interrupt (the same
        // semantics as Ctrl-C mid-turn — without this branch the signal
        // would be dropped while we await stdin).
        let Some(timeout) = self.ask_timeout else {
            return tokio::select! {
                line = self.stdin.recv() => reply_from_line(line.as_deref()),
                _ = self.sigint.recv() => {
                    eprintln!();
                    self.handle.interrupt();
                    AskReply::Deny { message: None }
                }
            };
        };
        tokio::select! {
            answered = tokio::time::timeout(timeout, self.stdin.recv()) => match answered {
                Ok(line) => reply_from_line(line.as_deref()),
                Err(_) => {
                    eprintln!("(no answer in {}s — denied)", timeout.as_secs());
                    AskReply::Deny { message: None }
                }
            },
            _ = self.sigint.recv() => {
                eprintln!();
                self.handle.interrupt();
                AskReply::Deny { message: None }
            }
        }
    }
}

/// Parse a permission answer line into an `AskReply` (T1): `y`/`yes` allows;
/// `n <reason>` / `no <reason>` denies with the reason for the model; anything
/// else (incl. a bare `n` or EOF) is a plain deny.
fn reply_from_line(line: Option<&str>) -> hotl_engine::AskReply {
    use hotl_engine::AskReply;
    let Some(t) = line.map(str::trim) else { return AskReply::Deny { message: None } };
    if matches!(t, "y" | "Y" | "yes") {
        return AskReply::Allow;
    }
    let message = t
        .strip_prefix("n ")
        .or_else(|| t.strip_prefix("no "))
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    AskReply::Deny { message }
}

/// The interactive ask timeout from `HOTL_ASK_TIMEOUT` (seconds): unset →
/// the 300s default; `0` → wait indefinitely (backgrounded/detached sessions).
fn ask_timeout_from_env(secrets: &dyn SecretStore, cfg: &crate::config::Config) -> Option<std::time::Duration> {
    let secs = secrets
        .get("HOTL_ASK_TIMEOUT")
        .and_then(|v| v.parse::<u64>().ok())
        .or(cfg.behavior.ask_timeout_secs);
    match secs {
        Some(0) => None,
        Some(n) => Some(std::time::Duration::from_secs(n)),
        None => Some(std::time::Duration::from_secs(ASK_TIMEOUT_SECS)),
    }
}

/// `(-p prompt, --json)`; `Err(exit_code)` on bad usage.
struct Args {
    prompt: Option<String>,
    json_events: bool,
    schema: Option<PathBuf>,
}

fn parse_args(args: Vec<String>) -> Result<Args, i32> {
    let mut prompt: Option<String> = None;
    let mut json_events = false;
    let mut schema: Option<PathBuf> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" | "--print" => prompt = iter.next(),
            "--json" => json_events = true,
            "--json-schema" => schema = iter.next().map(PathBuf::from),
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
    Ok(Args { prompt, json_events, schema })
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
fn engine_config(model: &str, secrets: &dyn SecretStore, cfg: &crate::config::Config) -> EngineConfig {
    let mut config = EngineConfig { model: model.to_string(), ..Default::default() };
    if let Some(window) = secrets.get("HOTL_CONTEXT_WINDOW").and_then(|v| v.parse().ok()).or(cfg.context.window) {
        config.context_window = window;
    }
    config.fast_model = secrets.get("HOTL_FAST_MODEL").or_else(|| cfg.provider.fast_model.clone());
    if let Some(t) = secrets.get("HOTL_EVICT_TOKENS").and_then(|v| v.parse().ok()).or(cfg.context.evict_tokens) {
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

type ProviderAndSource = (Arc<dyn hotl_provider::Provider>, Arc<dyn hotl_provider::key::KeySource>);
type SelectedProvider = (Arc<dyn hotl_provider::Provider>, String, Arc<dyn hotl_provider::key::KeySource>);

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
    let raw = secrets
        .get("HOTL_MODEL")
        .or_else(|| cfg.provider.model.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let (provider_name, model) = match raw.split_once('/') {
        Some((p, m)) => (p.to_ascii_lowercase(), m.to_string()),
        None => ("anthropic".to_string(), raw),
    };
    let (provider, source) = match provider_name.as_str() {
        "anthropic" => resolve_anthropic(cfg, secrets)?,
        "openai" | "oai" => resolve_openai(cfg, secrets)?,
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

fn resolve_anthropic(cfg: &crate::config::Config, secrets: &dyn SecretStore) -> Result<ProviderAndSource, String> {
    let key = secrets.get("ANTHROPIC_API_KEY");
    let source = key_source_for(cfg, secrets, key.clone());
    if !source.refreshable() && key.is_none() {
        return Err(
            "ANTHROPIC_API_KEY is not set and no api_key_helper is configured.\n\
             Export the key, set [provider] api_key_helper in config.toml, or select \
             another provider, e.g. HOTL_MODEL=openai/<model> (with OPENAI_API_KEY, or \
             HOTL_OPENAI_BASE_URL for a local endpoint). `hotl watch` needs no key."
                .to_string(),
        );
    }
    Ok((Arc::new(AnthropicProvider::new(source.clone())), source))
}

fn resolve_openai(cfg: &crate::config::Config, secrets: &dyn SecretStore) -> Result<ProviderAndSource, String> {
    let base = secrets
        .get("HOTL_OPENAI_BASE_URL")
        .or_else(|| cfg.provider.base_url.clone())
        .unwrap_or_else(|| hotl_provider_openai::DEFAULT_BASE_URL.to_string());
    let key = secrets.get("OPENAI_API_KEY");
    let source = key_source_for(cfg, secrets, key.clone());
    if !source.refreshable() && key.is_none() && base == hotl_provider_openai::DEFAULT_BASE_URL {
        return Err("OPENAI_API_KEY is not set (required for api.openai.com; keyless works \
                     only with HOTL_OPENAI_BASE_URL pointing at a local/compatible endpoint, \
                     e.g. http://localhost:11434/v1 for Ollama), or configure [provider] \
                     api_key_helper."
            .to_string());
    }
    // H-09: a bearer key over cleartext http:// to a non-loopback host
    // crosses the network unencrypted. Warn loudly (don't silently send
    // it); loopback http is the normal local-endpoint case. A helper-sourced
    // key (source.refreshable()) is just as real a bearer credential as the
    // static env key, so it must trip this warning too.
    if (key.is_some() || source.refreshable()) && cleartext_nonloopback(&base) {
        eprintln!(
            "hotl: WARNING — HOTL_OPENAI_BASE_URL is a non-loopback http:// URL and \
             OPENAI_API_KEY is set; the key will cross the network unencrypted. \
             Use https:// or an SSH tunnel."
        );
    }
    Ok((Arc::new(hotl_provider_openai::OpenAiCompatProvider::new(base, source.clone())), source))
}

/// A cleartext base URL pointing somewhere other than the local machine.
fn cleartext_nonloopback(base: &str) -> bool {
    let Some(rest) = base.strip_prefix("http://") else { return false };
    let host = rest.split(['/', ':']).next().unwrap_or("");
    !matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") && !host.is_empty()
}

pub(crate) fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hotl")
}

pub(crate) fn sessions_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hotl/sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory `SecretStore` for tests — no real env mutation, no races
    /// between tests running in parallel.
    #[derive(Default)]
    struct MapSecrets(std::collections::HashMap<String, String>);

    impl<const N: usize> From<[(&str, &str); N]> for MapSecrets {
        fn from(pairs: [(&str, &str); N]) -> Self {
            MapSecrets(pairs.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
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
        assert!(source.refreshable(), "helper must win over the static env key");
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
        assert!(!source.refreshable(), "empty api_key_helper must not activate the helper");
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
}
