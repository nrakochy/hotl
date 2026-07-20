//! The execute surface (0001 §M0/M1): a steering REPL and `-p` headless.
//!
//! The surface is a client of the session actor: it renders events, answers
//! asks, and turns typed lines into prompts (idle) or steers (mid-turn).

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use hotl_context::{load_system_prompt, project_instructions};
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::{Clock, EnvSecrets, SecretStore, SystemClock};
use hotl_provider_anthropic::{AnthropicProvider, DEFAULT_MODEL};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, sandbox, Registry};
use tokio::sync::mpsc;

const ASK_TIMEOUT_SECS: u64 = 300;

pub async fn agent_main(args: Vec<String>) -> i32 {
    let mut prompt: Option<String> = None;
    let mut json_events = false;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" | "--print" => prompt = iter.next(),
            "--json" => json_events = true,
            other => {
                eprintln!("hotl: unknown argument `{other}` (try --help)");
                return 2;
            }
        }
    }
    let headless = prompt.is_some();
    if headless && prompt.as_deref().map(str::trim).unwrap_or("").is_empty() {
        eprintln!("hotl: -p requires a prompt");
        return 2;
    }

    let secrets = EnvSecrets;
    let (provider, model) = match select_provider(&secrets) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("hotl: {msg}");
            return 1;
        }
    };

    let clock = Arc::new(SystemClock);
    let config_dir = config_dir();
    let system = load_system_prompt(&config_dir);
    let rules = Arc::new(Rules::load(&config_dir));
    let sandbox_status = sandbox::probe();
    let sandbox_enforced = matches!(sandbox_status, sandbox::SandboxStatus::Enforced(_));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let log = match SessionLog::create(&sessions_dir(), &model, None, Masker::from_env(), clock.now_ms()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl: could not create session log: {e}");
            return 1;
        }
    };
    let session_id = log.session_id.clone();

    let mut initial_items = Vec::new();
    if let Some(instructions) = project_instructions(&cwd) {
        initial_items.push(instructions);
    }

    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules,
        sandbox_enforced,
        clock,
        log,
        system,
        initial_items,
        config: EngineConfig { model: model.clone(), ..Default::default() },
    });

    let mut surface = Surface::new(handle, headless, json_events);

    if let Some(p) = prompt {
        surface.handle.prompt(p).await;
        return surface.run_until_idle().await;
    }

    println!(
        "hotl · {model} · session {session_id} · {}",
        match &sandbox_status {
            sandbox::SandboxStatus::Enforced(m) => format!("sandbox:{m}"),
            other => other.label(),
        }
    );
    println!("type to prompt · type mid-turn to steer · ctrl-c interrupts · ctrl-d exits");
    surface.repl().await
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
            EngineEvent::Ask { summary, protected_why, reply } => {
                let answer = self.ask_human(&summary, protected_why.as_deref()).await;
                let _ = reply.send(answer);
            }
            EngineEvent::TurnDone { outcome, usage } => {
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
        }
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
fn select_provider(
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
            Ok((Arc::new(hotl_provider_openai::OpenAiCompatProvider::new(base, key)), model))
        }
        other => Err(format!(
            "unknown provider `{other}` in HOTL_MODEL. Supported: anthropic/<model>, openai/<model> \
             (openai covers any OpenAI-compatible endpoint via HOTL_OPENAI_BASE_URL)."
        )),
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hotl")
}

fn sessions_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hotl/sessions")
}
