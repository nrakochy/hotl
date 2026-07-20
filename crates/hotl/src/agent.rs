//! The execute surface (0001 §M0/M1): a steering REPL and `-p` headless.
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
use tokio::sync::mpsc;

const ASK_TIMEOUT_SECS: u64 = 300;

/// Context inherited from an earlier session (`hotl resume` — M3b).
pub(crate) struct Resumed {
    pub parent_id: String,
    pub items: Vec<hotl_types::Item>,
}

pub async fn agent_main(args: Vec<String>) -> i32 {
    let (prompt, json_events) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    run_session(prompt, json_events, None).await
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
    let secrets = EnvSecrets;
    let (provider, model) = match select_provider(&secrets) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("hotl: {msg}");
            return 1;
        }
    };
    let scaffold = match scaffold(provider, model, &secrets) {
        Ok(s) => s,
        Err(code) => return code,
    };
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
        let log = SessionLog::create(&sessions_dir(), &scaffold.model, parent_id, Masker::from_env(), scaffold.clock.now_ms())
            .map_err(|e| format!("could not create session log: {e}"))?;
        let session_id = log.session_id.clone();
        let (snapshots, initial) = session_context(&session_id, &scaffold.cwd, &scaffold.config_dir, &resumed);
        Ok(spawn_session(scaffold.deps(log, snapshots, initial)))
    });

    crate::acp::serve(tokio::io::stdin(), tokio::io::stdout(), factory).await;
    0
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
}

fn scaffold(provider: Arc<dyn hotl_provider::Provider>, model: String, secrets: &dyn SecretStore) -> Result<Scaffold, i32> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let config_dir = config_dir();
    let system = load_system_prompt(&config_dir);
    let rules = load_rules(&config_dir);
    let sandbox_status = sandbox::probe();
    let sandbox_enforced = matches!(sandbox_status, sandbox::SandboxStatus::Enforced(_));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = engine_config(&model, secrets);
    let spawn_builder = child_builder(
        provider.clone(), rules.clone(), clock.clone(), config.clone(),
        cwd.clone(), config_dir.clone(), system.clone(), model.clone(), sandbox_enforced,
    );
    let registry = Arc::new(build_registry(&config_dir, Some(spawn_builder)));
    let hooks = load_hooks(&config_dir);
    Ok(Scaffold {
        provider, model, clock, config_dir, system, rules, sandbox_enforced,
        sandbox_status, cwd, config, registry, hooks,
    })
}

impl Scaffold {
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
    let (provider, model) = match select_provider(&secrets) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("hotl: {msg}");
            return 1;
        }
    };
    let scaffold = match scaffold(provider, model, &secrets) {
        Ok(s) => s,
        Err(code) => return code,
    };

    let parent_id = resumed.as_ref().map(|r| r.parent_id.clone());
    let log = match SessionLog::create(&sessions_dir(), &scaffold.model, parent_id, Masker::from_env(), scaffold.clock.now_ms()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl: could not create session log: {e}");
            return 1;
        }
    };
    let session_id = log.session_id.clone();
    spawn_secret_audit(log.path().to_path_buf());
    let (snapshots, initial_items) = session_context(&session_id, &scaffold.cwd, &scaffold.config_dir, &resumed);
    let handle = spawn_session(scaffold.deps(log, snapshots, initial_items));

    let mut surface = Surface::new(handle, headless, json_events);
    if let Some(p) = prompt {
        surface.handle.prompt(p).await;
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
    print_banner(&scaffold.model, &session_id, &scaffold.sandbox_status);
    surface.repl().await
}

/// Builtins + the `mcp` meta-tool (M3a) + the `spawn` tool (M4) when a child
/// builder is supplied. `spawn` is omitted for child sessions, so sub-agents
/// cannot recurse (structural depth cap — 0005 §3).
fn build_registry(
    config_dir: &std::path::Path,
    spawn_builder: Option<Arc<dyn crate::spawn::ChildBuilder>>,
) -> Registry {
    let mut registry = Registry::builtin_with(hotl_tools::diagnostics::Diagnostics::load(config_dir));
    let (mcp, warning) = hotl_mcp::config::load(config_dir);
    if let Some(warning) = warning {
        eprintln!("hotl: {warning}");
    }
    if !mcp.servers.is_empty() {
        let trust = hotl_mcp::trust::TrustStore::load(config_dir);
        registry.register(Box::new(hotl_mcp::McpTool::new(mcp.servers, trust)));
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
    config_dir: PathBuf,
    system: String,
    model: String,
    sandbox_enforced: bool,
}

impl crate::spawn::ChildBuilder for HotlChildBuilder {
    fn build(&self, _brief: &str) -> Result<hotl_engine::SessionHandle, String> {
        let log = SessionLog::create(&sessions_dir(), &self.model, None, Masker::from_env(), self.clock.now_ms())
            .map_err(|e| format!("child session log: {e}"))?;
        let registry = Registry::builtin_with(hotl_tools::diagnostics::Diagnostics::load(&self.config_dir));
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
    config_dir: PathBuf,
    system: String,
    model: String,
    sandbox_enforced: bool,
) -> Arc<dyn crate::spawn::ChildBuilder> {
    Arc::new(HotlChildBuilder {
        provider,
        rules,
        clock,
        config,
        cwd,
        config_dir,
        system,
        model,
        sandbox_enforced,
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

/// Lane-2 shell hooks from `hooks.toml`, or None (M5).
fn load_hooks(config_dir: &std::path::Path) -> Option<Arc<dyn hotl_engine::hooks::Hooks>> {
    crate::shell_hooks::load(config_dir)
        .map(|h| Arc::new(h) as Arc<dyn hotl_engine::hooks::Hooks>)
}

fn load_rules(config_dir: &std::path::Path) -> Arc<Rules> {
    let (rules, warning) = Rules::load(config_dir);
    if let Some(warning) = warning {
        eprintln!("hotl: {warning}");
    }
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
}

impl Surface {
    fn new(handle: SessionHandle, headless: bool, json: bool) -> Self {
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
        Self { handle, headless, json, stdin: rx, turn_running: false, saw_text: false }
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
                        self.handle.steer(line).await;
                        eprintln!("(steered — woven into the agent's next step)");
                    } else {
                        self.handle.prompt(line).await;
                        self.turn_running = true;
                    }
                }
                _ = tokio::signal::ctrl_c() => {
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
                _ = tokio::signal::ctrl_c() => self.handle.interrupt(),
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
                let _ = reply.send(false);
                serde_json::json!({"type":"ask_denied","summary":summary})
            }
            EngineEvent::TurnDone { outcome, usage } => {
                self.turn_running = false;
                serde_json::json!({"type":"turn_done","outcome":format!("{outcome:?}"),"usage":usage})
            }
        };
        println!("{v}");
    }

    async fn ask_human(&mut self, summary: &str, protected_why: Option<&str>) -> bool {
        if self.headless || !std::io::stdin().is_terminal() {
            eprintln!("hotl: denied (headless): {summary}");
            return false;
        }
        if let Some(why) = protected_why {
            eprintln!("⚠ PROTECTED PATH — {why}");
        }
        eprint!("allow {summary}? [y/N] ");
        match tokio::time::timeout(std::time::Duration::from_secs(ASK_TIMEOUT_SECS), self.stdin.recv()).await {
            Ok(Some(line)) => matches!(line.trim(), "y" | "Y" | "yes"),
            Ok(None) => false,
            Err(_) => {
                eprintln!("(no answer in {ASK_TIMEOUT_SECS}s — denied)");
                false
            }
        }
    }
}

/// `(-p prompt, --json)`; `Err(exit_code)` on bad usage.
fn parse_args(args: Vec<String>) -> Result<(Option<String>, bool), i32> {
    let mut prompt: Option<String> = None;
    let mut json_events = false;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" | "--print" => prompt = iter.next(),
            "--json" => json_events = true,
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
    Ok((prompt, json_events))
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
fn engine_config(model: &str, secrets: &dyn SecretStore) -> EngineConfig {
    let mut config = EngineConfig { model: model.to_string(), ..Default::default() };
    if let Some(window) = secrets.get("HOTL_CONTEXT_WINDOW").and_then(|v| v.parse().ok()) {
        config.context_window = window;
    }
    config.fast_model = secrets.get("HOTL_FAST_MODEL");
    // M4/#9: opt into fresh-slate compaction and hiding the context gauge.
    config.compaction_reset = secrets.get("HOTL_COMPACTION_RESET").as_deref() == Some("1");
    config.show_context_pct = secrets.get("HOTL_HIDE_CONTEXT_PCT").as_deref() != Some("1");
    config
}

fn exit_code(outcome: &Outcome) -> i32 {
    match outcome {
        Outcome::Done { .. } => 0,
        Outcome::Cancelled => 130,
        _ => 1,
    }
}

/// Provider/model selection. `HOTL_MODEL` accepts `provider/model`:
///   anthropic/claude-…   needs ANTHROPIC_API_KEY
///   openai/gpt-…         needs OPENAI_API_KEY, or HOTL_OPENAI_BASE_URL for
///                        keyless OpenAI-compatible endpoints (Ollama etc.)
/// A bare model string means Anthropic; unset means the Anthropic default.
pub(crate) fn select_provider(
    secrets: &dyn SecretStore,
) -> Result<(Arc<dyn hotl_provider::Provider>, String), String> {
    let raw = secrets.get("HOTL_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let (provider_name, model) = match raw.split_once('/') {
        Some((p, m)) => (p.to_ascii_lowercase(), m.to_string()),
        None => ("anthropic".to_string(), raw),
    };
    match provider_name.as_str() {
        "anthropic" => {
            let key = secrets.get("ANTHROPIC_API_KEY").ok_or_else(|| {
                "ANTHROPIC_API_KEY is not set.\n\
                 Export it, or select another provider, e.g. HOTL_MODEL=openai/<model> \
                 (with OPENAI_API_KEY, or HOTL_OPENAI_BASE_URL for a local endpoint). \
                 `hotl watch` needs no key."
                    .to_string()
            })?;
            Ok((Arc::new(AnthropicProvider::new(key)), model))
        }
        "openai" | "oai" => {
            let base = secrets
                .get("HOTL_OPENAI_BASE_URL")
                .unwrap_or_else(|| hotl_provider_openai::DEFAULT_BASE_URL.to_string());
            let key = secrets.get("OPENAI_API_KEY");
            if key.is_none() && base == hotl_provider_openai::DEFAULT_BASE_URL {
                return Err("OPENAI_API_KEY is not set (required for api.openai.com; \
                            keyless works only with HOTL_OPENAI_BASE_URL pointing at a \
                            local/compatible endpoint, e.g. http://localhost:11434/v1 for Ollama)."
                    .to_string());
            }
            // H-09: a bearer key over cleartext http:// to a non-loopback host
            // crosses the network unencrypted. Warn loudly (don't silently
            // send it); loopback http is the normal local-endpoint case.
            if key.is_some() && cleartext_nonloopback(&base) {
                eprintln!(
                    "hotl: WARNING — HOTL_OPENAI_BASE_URL is a non-loopback http:// URL and \
                     OPENAI_API_KEY is set; the key will cross the network unencrypted. \
                     Use https:// or an SSH tunnel."
                );
            }
            Ok((Arc::new(hotl_provider_openai::OpenAiCompatProvider::new(base, key)), model))
        }
        other => Err(format!(
            "unknown provider `{other}` in HOTL_MODEL. Supported: anthropic/<model>, openai/<model> \
             (openai covers any OpenAI-compatible endpoint via HOTL_OPENAI_BASE_URL)."
        )),
    }
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
