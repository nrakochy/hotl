//! The execute surface: REPL and `-p` headless (0001 §M0).

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use futures_util::future::BoxFuture;
use hotl_context::{load_system_prompt, project_instructions};
use hotl_engine::{Engine, EngineConfig, EngineEvent, Outcome};
use hotl_platform::{Clock, EnvSecrets, SecretStore, SystemClock};
use hotl_provider_anthropic::{AnthropicProvider, DEFAULT_MODEL};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{PermissionGate, Registry};
use tokio_util::sync::CancellationToken;

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
    let (provider, model): (Box<dyn hotl_provider::Provider>, String) =
        match select_provider(&secrets) {
            Ok(pair) => pair,
            Err(msg) => {
                eprintln!("hotl: {msg}");
                return 1;
            }
        };
    let provider = &*provider;
    let registry = Registry::builtin();
    let clock = SystemClock;
    let config_dir = config_dir();
    let system = load_system_prompt(&config_dir);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let mut log = match SessionLog::create(&sessions_dir(), &model, None, Masker::from_env(), clock.now_ms()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl: could not create session log: {e}");
            return 1;
        }
    };

    let mut items = Vec::new();
    if let Some(instructions) = project_instructions(&cwd) {
        items.push(instructions);
    }

    let gate = CliGate { headless };
    let engine = Engine {
        provider,
        registry: &registry,
        gate: &gate,
        clock: &clock,
        config: EngineConfig { model: model.clone(), ..Default::default() },
    };

    if let Some(p) = prompt {
        return run_one(&engine, &mut items, &mut log, &system, p, json_events).await;
    }

    // REPL
    println!("hotl · {model} · session {} · ctrl-c interrupts a turn, ctrl-d exits", log.session_id);
    loop {
        eprint!("\n❯ ");
        let Some(line) = read_stdin_line().await else {
            println!();
            return 0; // EOF
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if matches!(line.as_str(), "exit" | "quit") {
            return 0;
        }
        run_one(&engine, &mut items, &mut log, &system, line, false).await;
    }
}

async fn run_one(
    engine: &Engine<'_>,
    items: &mut Vec<hotl_types::Item>,
    log: &mut SessionLog,
    system: &str,
    prompt: String,
    json_events: bool,
) -> i32 {
    let cancel = CancellationToken::new();
    let cancel_on_sigint = cancel.clone();
    let ctrlc = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_on_sigint.cancel();
        }
    });

    let mut saw_text = false;
    let mut on_event = |event: EngineEvent| {
        if json_events {
            let v = match &event {
                EngineEvent::TextDelta(t) => serde_json::json!({"type":"text_delta","text":t}),
                EngineEvent::ThinkingDelta(_) => serde_json::json!({"type":"thinking_delta"}),
                EngineEvent::ToolStart { name, summary } => serde_json::json!({"type":"tool_start","name":name,"summary":summary}),
                EngineEvent::ToolDone { name, ok } => serde_json::json!({"type":"tool_done","name":name,"ok":ok}),
                EngineEvent::ToolDenied { name } => serde_json::json!({"type":"tool_denied","name":name}),
                EngineEvent::Retrying { attempt, reason } => serde_json::json!({"type":"retrying","attempt":attempt,"reason":reason}),
                EngineEvent::TurnDone { usage } => serde_json::json!({"type":"turn_done","usage":usage}),
            };
            println!("{v}");
            return;
        }
        match event {
            EngineEvent::TextDelta(t) => {
                saw_text = true;
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            EngineEvent::ToolStart { summary, .. } => {
                if saw_text {
                    println!();
                    saw_text = false;
                }
                eprintln!("· {summary}");
            }
            EngineEvent::ToolDone { ok, .. } => {
                if !ok {
                    eprintln!("  (tool reported an error — feeding it back to the model)");
                }
            }
            EngineEvent::ToolDenied { .. } => eprintln!("  (denied)"),
            EngineEvent::Retrying { attempt, reason } => eprintln!("· retrying ({attempt}): {reason}"),
            EngineEvent::ThinkingDelta(_) => {}
            EngineEvent::TurnDone { usage } => {
                eprintln!(
                    "\n[in {} out {} cache-read {}]",
                    usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens
                );
            }
        }
    };

    let result = engine
        .run_prompt(items, log, system, prompt, cancel.clone(), &mut on_event)
        .await;
    ctrlc.abort();

    match result {
        Ok(Outcome::Done { .. }) => 0,
        Ok(Outcome::Cancelled) => {
            eprintln!("\n(interrupted)");
            130
        }
        Ok(Outcome::TurnLimit) => {
            eprintln!("\nhotl: stopped at max_turns — the task didn't converge; break it into smaller prompts.");
            1
        }
        Ok(Outcome::Refused) => {
            eprintln!("\nhotl: the model declined this request (safety classifiers).");
            1
        }
        Err(e) => {
            eprintln!("\nhotl: {e}");
            1
        }
    }
}

/// Provider/model selection (distribution.md §D5 resolution order applies to
/// keys). `HOTL_MODEL` accepts `provider/model`:
///   anthropic/claude-…   needs ANTHROPIC_API_KEY
///   openai/gpt-…         needs OPENAI_API_KEY, or HOTL_OPENAI_BASE_URL for
///                        keyless OpenAI-compatible endpoints (Ollama etc.)
/// A bare model string means Anthropic; unset means the Anthropic default.
fn select_provider(secrets: &dyn SecretStore) -> Result<(Box<dyn hotl_provider::Provider>, String), String> {
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
            Ok((Box::new(AnthropicProvider::new(key)), model))
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
            Ok((Box::new(hotl_provider_openai::OpenAiCompatProvider::new(base, key)), model))
        }
        other => Err(format!(
            "unknown provider `{other}` in HOTL_MODEL. Supported: anthropic/<model>, openai/<model> \
             (openai covers any OpenAI-compatible endpoint via HOTL_OPENAI_BASE_URL)."
        )),
    }
}

/// The human on the loop, CLI edition. Headless (`-p`) default-denies without
/// waiting (Sec #11); interactive asks time out to deny after 5 minutes so an
/// unattended terminal can't hold a turn open forever.
struct CliGate {
    headless: bool,
}

impl PermissionGate for CliGate {
    fn ask<'a>(&'a self, summary: &'a str, protected_why: Option<&'a str>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            if self.headless || !std::io::stdin().is_terminal() {
                eprintln!("hotl: denied (headless): {summary}");
                return false;
            }
            if let Some(why) = protected_why {
                eprintln!("⚠ PROTECTED PATH — {why}");
            }
            eprint!("allow {summary}? [y/N] ");
            let answer = tokio::time::timeout(
                std::time::Duration::from_secs(ASK_TIMEOUT_SECS),
                read_stdin_line(),
            )
            .await;
            match answer {
                Ok(Some(line)) => matches!(line.trim(), "y" | "Y" | "yes"),
                Ok(None) => false,
                Err(_) => {
                    eprintln!("(no answer in {ASK_TIMEOUT_SECS}s — denied)");
                    false
                }
            }
        })
    }
}

async fn read_stdin_line() -> Option<String> {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    match reader.read_line(&mut line).await {
        Ok(0) => None,
        Ok(_) => Some(line),
        Err(_) => None,
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
