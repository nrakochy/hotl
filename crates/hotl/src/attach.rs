//! `hotl attach [id]` — connect to a detached session's unix socket and drive
//! it from your terminal (the human client of `hotl serve`). Renders the
//! session's output, forwards what you type (prompt when idle, steer mid-turn),
//! and answers parked permission asks. `Ctrl-D` detaches (the session lives on);
//! `/stop` shuts the session down. Bare `hotl attach` lists live sessions.

use hotl_tui::client::{AcpClient, ServerMsg};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::session_server::list_live;
use hotl_platform::Ipc as _;

type WriteHalf = tokio::io::WriteHalf<<hotl_platform::ActiveIpc as hotl_platform::Ipc>::Stream>;
type ReadHalf = tokio::io::ReadHalf<<hotl_platform::ActiveIpc as hotl_platform::Ipc>::Stream>;
type Client = AcpClient<WriteHalf>;
type Lines = tokio::io::Lines<BufReader<ReadHalf>>;

pub async fn attach_main(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--json") {
        for line in live_json() {
            println!("{line}");
        }
        return 0;
    }
    let Some(id) = args.first().map(String::as_str) else {
        return list_sessions();
    };
    if hotl_platform::IPC.liveness(id) != hotl_platform::Liveness::Live {
        // Allow a prefix, like `hotl resume`.
        match list_live().into_iter().find(|s| s.starts_with(id)) {
            Some(full) => return connect(&full).await,
            None => {
                eprintln!("hotl attach: no live session `{id}` (bare `hotl attach` lists them)");
                return 1;
            }
        }
    }
    connect(id).await
}

fn list_sessions() -> i32 {
    let live = list_live();
    if live.is_empty() {
        println!("no backgrounded sessions. start one with `hotl bg [prompt]`.");
    } else {
        println!("live backgrounded sessions (attach with `hotl attach <id>`):");
        for id in live {
            println!("  {id}");
        }
    }
    0
}

pub(crate) fn live_json() -> Vec<String> {
    crate::run_status::read_live()
        .iter()
        .filter_map(|status| serde_json::to_string(status).ok())
        .collect()
}

pub(crate) fn initialize_params(token: &str, client: &str) -> Value {
    json!({
        "protocolVersion": 1,
        "clientInfo": {"name": client, "version": env!("CARGO_PKG_VERSION")},
        "clientCapabilities": {
            "session": {"notices": {}},
            "elicitation": {"form": {}},
            "_meta": {"hotl.dev/extensions": 1},
        },
        "_meta": {crate::session_server::META_SOCKET_TOKEN: token},
    })
}

pub(crate) enum Opened {
    Attached,
    Refused(String),
    Closed,
}

pub(crate) async fn open(client: &mut Client, lines: &mut Lines, id: &str, token: &str) -> Opened {
    let Ok(init) = client
        .request("initialize", initialize_params(token, "hotl-attach"))
        .await
    else {
        return Opened::Closed;
    };
    match response(lines, init).await {
        Some(Ok(_)) => {}
        Some(Err(e)) => return Opened::Refused(e),
        None => return Opened::Closed,
    }
    client.set_session_id(id);
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let Ok(resume) = client.request("session/resume", json!({"cwd": cwd})).await else {
        return Opened::Closed;
    };
    match response(lines, resume).await {
        Some(Ok(_)) => Opened::Attached,
        Some(Err(e)) => Opened::Refused(e),
        None => Opened::Closed,
    }
}

async fn response(lines: &mut Lines, id: u64) -> Option<Result<Value, String>> {
    loop {
        let line = lines.next_line().await.ok()??;
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(ServerMsg::Response { id: got, result }) = hotl_tui::client::decode(&msg) {
            if got == id {
                return Some(result);
            }
        }
    }
}

async fn connect(id: &str) -> i32 {
    let stream = match hotl_platform::IPC.connect(id).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hotl attach: could not connect to `{id}`: {e} (is it still running?)");
            return 1;
        }
    };
    let (read, write) = tokio::io::split(stream);
    // Authenticate with the per-session token before anything else (Vuln 1).
    let token = match std::fs::read_to_string(crate::session_server::token_path(id)) {
        Ok(token) => token,
        Err(e) => {
            eprintln!("hotl attach: no session token for `{id}` ({e}); is it still running?");
            return 1;
        }
    };
    let mut client = AcpClient::new(write);
    let mut server = BufReader::new(read).lines();
    match open(&mut client, &mut server, id, token.trim()).await {
        Opened::Attached => {}
        Opened::Refused(e) => {
            eprintln!("hotl attach: `{id}` refused the attach: {e}");
            return 1;
        }
        Opened::Closed => {
            println!("\n(session closed)");
            return 0;
        }
    }
    println!("attached to {id} — type to prompt · Ctrl-D detaches · /stop ends the session");

    // Terminal stdin on a blocking thread → channel (same shape as the REPL).
    let (tx, mut stdin_rx) = mpsc::channel::<String>(8);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.blocking_send(line.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut state = Attached::default();
    let (shell_tx, mut shell_rx) = mpsc::unbounded_channel::<ShellDone>();
    loop {
        tokio::select! {
            line = server.next_line() => match line {
                Ok(Some(line)) => {
                    if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                        state.on_server(&msg);
                    }
                }
                _ => { println!("\n(session closed)"); return 0; }
            },
            line = stdin_rx.recv() => {
                let Some(line) = line else { return detach(&mut client).await };
                if on_input(line.trim(), &mut client, &mut state, &shell_tx).await {
                    return 0;
                }
            }
            done = shell_rx.recv() => {
                if let Some(done) = done {
                    on_shell_done(done, &mut client, &mut state).await;
                }
            }
        }
    }
}

#[derive(Default)]
struct Attached {
    pending_ask: Option<Parked>,
    turn_running: bool,
    prompts: Vec<u64>,
    shell_running: bool,
}

type ShellDone = (String, String, Value);

static MID_LINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn set_mid_line(on: bool) {
    MID_LINE.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn fresh_line() {
    if MID_LINE.swap(false, std::sync::atomic::Ordering::Relaxed) {
        println!();
    }
}

fn shell_text(command: &str, shown: &str, mid_line: bool) -> String {
    let mut out = String::new();
    if mid_line {
        out.push('\n');
    }
    out.push_str(&format!("$ {command}\n"));
    if !shown.is_empty() {
        out.push_str(shown);
        out.push('\n');
    }
    out
}

fn spawn_bang(command: String, cwd: std::path::PathBuf, tx: mpsc::UnboundedSender<ShellDone>) {
    tokio::spawn(async move {
        let (shown, params) = bang(&command, &cwd).await;
        let _ = tx.send((command, shown, params));
    });
}

impl Attached {
    fn on_server(&mut self, msg: &Value) {
        if msg
            .pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            == Some("usage_update")
        {
            if self.prompts.is_empty() && self.turn_running {
                self.turn_running = false;
                show_prompt();
            }
            return;
        }
        let Some(msg) = hotl_tui::client::decode(msg) else {
            return;
        };
        if let Some(running) = render(msg, &mut self.pending_ask, &mut self.prompts) {
            self.turn_running = running;
        }
    }
}

/// What a typed line means: answer a pending ask, steer a running turn, start
/// a new turn, or `/stop`. Pure, so the decision is testable apart from the
/// socket write it implies.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Stop,
    Nothing,
    AnswerAsk { parked: Parked, allow: bool },
    Steer(String),
    Prompt(String),
    Shell(String),
    ShellUsage,
}

/// A prompt parked on the human, and which reply method answers it. Tool asks
/// and egress asks (plan 0026) draw from one id counter but carry different
/// reply types, so the kind has to travel with the id — answering one through
/// the other's method would silently mis-route.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parked {
    id: u64,
    /// `Some(host)` for an egress ask, `None` for a tool ask. The host rides
    /// along so the paste-able config line can name it after a `y`.
    egress: Option<String>,
    prompt: String,
}

/// Classify a typed line. A parked ask consumes the next line as its answer —
/// anything that is not an affirmative denies, which is the safe default.
fn route(text: &str, turn_running: bool, pending_ask: Option<&Parked>) -> Action {
    if text == "/stop" {
        return Action::Stop;
    }
    if text.is_empty() {
        return Action::Nothing;
    }
    if let Some(rest) = text.trim_start().strip_prefix('!') {
        let command = rest.trim();
        if command.is_empty() {
            return Action::ShellUsage;
        }
        return Action::Shell(command.to_string());
    }
    if let Some(parked) = pending_ask {
        return Action::AnswerAsk {
            parked: parked.clone(),
            allow: matches!(text, "y" | "Y" | "yes"),
        };
    }
    if turn_running {
        Action::Steer(text.to_string())
    } else {
        Action::Prompt(text.to_string())
    }
}

async fn bang(command: &str, cwd: &std::path::Path) -> (String, Value) {
    let run = hotl_tools::user_shell::run(
        command,
        cwd,
        hotl_tools::user_shell::USER_SHELL_TIMEOUT,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let append = |text: &str, label: &str| {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(label);
        out
    };
    let shown = match run.end.label() {
        Some(label) => append(&run.output, &label),
        None => run.output.clone(),
    };
    let output = match run.end.shared_label() {
        Some(label) => append(&run.output, &label),
        None => run.output,
    };
    let params = json!({
        "command": command,
        "output": output,
        "exitCode": run.end.exit_code(),
        "end": run.end.wire(),
    });
    (shown, params)
}

async fn on_input(
    text: &str,
    client: &mut Client,
    state: &mut Attached,
    shell_tx: &mpsc::UnboundedSender<ShellDone>,
) -> bool {
    match route(text, state.turn_running, state.pending_ask.as_ref()) {
        Action::Stop => {
            let _ = client.request("_hotl/session/stop", json!({})).await;
            true
        }
        Action::Nothing => false,
        Action::ShellUsage => {
            eprintln!("usage: !<command> — runs it here and shares the output with the model");
            false
        }
        Action::Shell(command) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            if state.shell_running {
                eprintln!("a ! command is already running");
                return false;
            }
            state.shell_running = true;
            fresh_line();
            println!("[running !{command}…]");
            spawn_bang(command, cwd, shell_tx.clone());
            false
        }
        Action::AnswerAsk { parked, allow } => {
            state.pending_ask = None;
            set_mid_line(false);
            let _ = match &parked.egress {
                Some(_) => client.reply_egress(parked.id, allow).await,
                None => client.reply_permission(parked.id, allow, false, None).await,
            };
            if let (Some(host), true) = (&parked.egress, allow) {
                eprintln!("{}", crate::session_server::paste_hint(host));
            }
            false
        }
        // Expansion happens here, not in `route`: reading files is I/O, and
        // the transcript should show what the human typed.
        Action::Steer(text) => {
            let text = crate::setup::expand_file_refs(&text);
            let _ = client
                .request("_hotl/session/steer", json!({"text": text}))
                .await;
            false
        }
        Action::Prompt(text) => {
            let text = crate::setup::expand_file_refs(&text);
            if let Ok(id) = client
                .request(
                    "session/prompt",
                    json!({"prompt": [{"type": "text", "text": text}]}),
                )
                .await
            {
                state.prompts.push(id);
                state.turn_running = true;
            }
            false
        }
    }
}

async fn on_shell_done(done: ShellDone, client: &mut Client, state: &mut Attached) {
    let (command, shown, params) = done;
    let mid = MID_LINE.swap(false, std::sync::atomic::Ordering::Relaxed);
    print!("{}", shell_text(&command, &shown, mid));
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let _ = client.request("_hotl/session/shell_note", params).await;
    state.shell_running = false;
    if let Some(parked) = &state.pending_ask {
        eprint!("{}", parked.prompt);
        let _ = std::io::Write::flush(&mut std::io::stderr());
        set_mid_line(true);
    } else if !state.turn_running {
        show_prompt();
    }
}

fn show_prompt() {
    eprint!("\n❯ ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

fn turn_end_kind(result: &Result<Value, String>) -> &str {
    let Ok(v) = result else {
        return "error";
    };
    if let Some(kind) = v
        .pointer("/_meta/hotl.dev~1turn/outcome/kind")
        .and_then(Value::as_str)
    {
        return kind;
    }
    match v.get("stopReason").and_then(Value::as_str) {
        Some("end_turn") => "done",
        Some("cancelled") => "cancelled",
        Some("max_turn_requests") => "turn_limit",
        Some("refusal") => "refused",
        _ => "error",
    }
}

/// Render a server message; returns whether it says a turn is running, or
/// `None` when it says nothing either way.
fn render(
    msg: ServerMsg,
    pending_ask: &mut Option<Parked>,
    prompts: &mut Vec<u64>,
) -> Option<bool> {
    match msg {
        ServerMsg::PermissionRequest {
            req_id,
            summary,
            protected_why,
            ..
        } => {
            if let Some(why) = protected_why {
                eprintln!("⚠ PROTECTED PATH — {why}");
            }
            let prompt = format!("allow {summary}? [y/N] ");
            eprint!("{prompt}");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            set_mid_line(true);
            *pending_ask = Some(Parked {
                id: req_id,
                egress: None,
                prompt,
            });
            Some(true)
        }
        // Deliberately different wording and framing from the tool ask above:
        // a human who just approved `npm install` must not read this as a
        // duplicate and reflex-key it.
        ServerMsg::EgressRequest { req_id, host } => {
            eprintln!("\nnetwork: reaching \"{host}\" was not in the approved command");
            eprintln!("this host is not in [network].allow");
            let prompt = "allow for this session? [y/N] ".to_string();
            eprint!("{prompt}");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            set_mid_line(true);
            *pending_ask = Some(Parked {
                id: req_id,
                egress: Some(host),
                prompt,
            });
            Some(true)
        }
        ServerMsg::QuestionRequest { .. } => Some(true),
        ServerMsg::Update(update) => {
            render_update(&update);
            Some(true)
        }
        ServerMsg::Response { id, result } => {
            let Some(pos) = prompts.iter().position(|p| *p == id) else {
                if let Err(e) = result {
                    eprintln!("\n[{e}]");
                }
                return None;
            };
            prompts.remove(pos);
            let kind = turn_end_kind(&result);
            if kind != "done" {
                eprintln!("\n[turn ended: {kind}]");
            }
            show_prompt();
            Some(!prompts.is_empty())
        }
    }
}

/// One human-readable line for a `session/update` frame, or `None` for the
/// frames that are not lines: `text_delta` streams to stdout unadorned (the
/// answer itself), and `tool_done` says nothing when the tool succeeded.
///
/// Every `type` `wire::update_frame` can produce is handled here. `_ => None`
/// is reserved for deliberate omissions, each named — attach used to cover 4
/// of 13 variants and silently dropped the rest (§7).
/// INVARIANT: no streamable variant is dropped. Enforced by
/// `attach_renders_every_update_variant`.
fn update_line(update: &Value) -> Option<String> {
    let s = |key: &str| update.get(key).and_then(Value::as_str).unwrap_or("");
    let n = |key: &str| update.get(key).and_then(Value::as_u64).unwrap_or(0);
    Some(
        match update.get("type").and_then(Value::as_str).unwrap_or("") {
            // Streamed to stdout by the caller, not as a labelled line.
            "text_delta" => return None,
            "thinking_delta" => format!("· thinking: {}", s("text")),
            "tool_start" => format!("\n· {}", s("summary")),
            "tool_done" => {
                if update.get("ok").and_then(Value::as_bool) == Some(false) {
                    "  (tool error — fed back to the model)".to_string()
                } else {
                    // A tool that worked needs no line; the output speaks.
                    return None;
                }
            }
            // 0061 decision 15: plain `attach` is a line printer, so a frame
            // every 250 ms would be one line per output line — spam, not
            // liveness. The console renders these on the card instead.
            "tool_progress" | "tool_input_progress" => return None,
            "compacting" => format!("(folding history — {} items)", n("items")),
            "goal_evaluating" => format!("· evaluating goal (turn {})", n("turn")),
            // 0061 T26: the same text the console shows.
            // 0061 T26: `-p --json` only today — the session server turns an
            // `Ask` into its own `ask` frame — but the text is the console's,
            // so a future broadcast reads the same on both surfaces.
            "ask_denied" => format!(
                "· ask skipped — no human was attached to answer: {}",
                s("summary")
            ),
            "question_no_human" => format!(
                "· question skipped — no human was attached to answer: {}",
                s("header")
            ),
            "tool_queued" => format!("· queued: {} (behind {})", s("summary"), n("ahead")),
            // Mirrors `tool_done`'s success exemption: a child that worked
            // needs no line (0039).
            "child_tool" => match update.get("ok").and_then(Value::as_bool) {
                None => format!("  ↳ {}", s("summary")),
                Some(false) => "  ↳ (sub-agent tool error)".to_string(),
                Some(true) => return None,
            },
            "tool_denied" => format!("· denied: {}", s("name")),
            "tool_auto_allowed" => format!("· auto-allowed {} (rule: {})", s("name"), s("rule")),
            "tool_flagged" => {
                if update.get("denied").and_then(Value::as_bool) == Some(true) {
                    format!("⚑ refused: {} — {}", s("summary"), s("why"))
                } else {
                    format!("⚑ allowed with notice: {} — {}", s("summary"), s("why"))
                }
            }
            "retrying" => {
                // 0061 T15: the countdown is the liveness — nothing is
                // computing during a backoff. A scope off `sample` says whose
                // ladder it is; the sample one stays as it read before.
                let mut line = match update.get("max").and_then(Value::as_u64) {
                    Some(max) => format!("· retrying ({}/{max})", n("attempt")),
                    None => format!("· retrying (attempt {})", n("attempt")),
                };
                if let Some(scope) = update.get("scope").and_then(Value::as_str) {
                    if scope != "sample" {
                        line.push_str(&format!(" [{scope}]"));
                    }
                }
                line.push_str(&format!(" — {}", s("reason")));
                if let Some(ms) = update.get("delay_ms").and_then(Value::as_u64) {
                    if ms > 0 {
                        line.push_str(&format!(" · in {}s", ms.div_ceil(1000)));
                    }
                }
                line
            }
            "fallback_model" => format!("· model fallback → {}", s("model")),
            "prompt_queued" => "· queued".to_string(),
            "fold_skipped" => format!("(fold skipped: {})", s("reason")),
            "model_changed" => format!("· model → {} (was {})", s("model"), s("previous")),
            "compacted" => {
                let focus = update
                    .get("focus")
                    .and_then(Value::as_str)
                    .map(|f| format!(" — focus: {f}"))
                    .unwrap_or_default();
                if update.get("degraded").and_then(Value::as_bool) == Some(true) {
                    format!("(context compacted — degraded{focus})")
                } else {
                    format!("(context compacted{focus})")
                }
            }
            "cleared" => format!("(cleared {} old tool results)", n("count")),
            "todos_changed" => {
                let n = update
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                format!("· todos: {n} item(s)")
            }
            "goal_changed" => match update.get("goal").and_then(Value::as_str) {
                Some(goal) => format!("· goal set: {goal}"),
                None => "· goal cleared".to_string(),
            },
            "memory_changed" => format!("· remembered ({} B): {}", n("bytes"), s("note")),
            "job_started" => format!(
                "· job {} started (pid {}): {}",
                s("job"),
                n("pid"),
                s("command")
            ),
            "job_done" => format!("· job {} ended · rc {} · {}s", s("job"), s("rc"), n("secs")),
            "memory_report" => {
                let entries = update
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                format!(
                    "· memory ({}): {entries} note(s), {} of {} B — {}",
                    s("mode"),
                    n("bytes"),
                    n("cap"),
                    s("path")
                )
            }
            "goal_verdict" => {
                let mut line = format!(
                    "· goal check (turn {}): {} — {}",
                    n("turns"),
                    s("verdict"),
                    s("reason")
                );
                // Present since 0051; an older server omits it.
                if let Some(usage) = update.get("usage") {
                    let t = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
                    line.push_str(&format!(
                        " · {} in / {} out",
                        t("input_tokens"),
                        t("output_tokens")
                    ));
                }
                line
            }
            "mode_changed" => format!("· permission mode → {}", s("mode")),
            "server_notice" => format!("· {}", s("text")),
            "cache_warm" => {
                let read = update
                    .get("usage")
                    .and_then(|u| u.get("cache_read_input_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                match update.get("cost_usd").and_then(Value::as_f64) {
                    Some(c) => format!("· cache kept warm ({read} read, ${c:.2})"),
                    None => format!("· cache kept warm ({read} read)"),
                }
            }
            _ => return None,
        },
    )
}

fn render_update(update: &Value) {
    if update.get("type").and_then(Value::as_str) == Some("text_delta") {
        let text = update.get("text").and_then(Value::as_str).unwrap_or("");
        print!("{text}");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        if !text.is_empty() {
            set_mid_line(!text.ends_with('\n'));
        }
        return;
    }
    if let Some(line) = update_line(update) {
        eprintln!("{line}");
        set_mid_line(false);
    }
}

async fn detach(client: &mut Client) -> i32 {
    let _ = client.request("session/close", json!({})).await;
    println!("\n(detached — session still running; `hotl attach` to return)");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_engine::EngineEvent;

    /// Every streamable engine event, for coverage assertions.
    fn every_streamable_event() -> Vec<EngineEvent> {
        vec![
            EngineEvent::TextDelta("hi".into()),
            EngineEvent::ThinkingDelta("mm".into()),
            EngineEvent::ToolStart {
                id: "t1".into(),
                name: "read".into(),
                summary: "read ./x".into(),
                input: serde_json::Value::Null,
            },
            EngineEvent::ToolDone {
                id: "t1".into(),
                name: "read".into(),
                ok: true,
                lines: 3,
                bytes: 5,
                output: String::new(),
                before: None,
            },
            EngineEvent::ToolProgress {
                id: "t1".into(),
                name: "bash".into(),
                tail: "Compiling hotl-engine".into(),
                lines: 12,
                bytes: 480,
            },
            EngineEvent::ToolInputProgress {
                calls: 1,
                bytes: 4096,
            },
            EngineEvent::ToolQueued {
                id: "t1".into(),
                name: "bash".into(),
                summary: "bash: cargo test".into(),
                ahead: 2,
            },
            EngineEvent::GoalEvaluating { turn: 3 },
            EngineEvent::Compacting { items: 42 },
            EngineEvent::ToolDenied {
                id: "t2".into(),
                name: "write".into(),
            },
            EngineEvent::ToolAutoAllowed {
                id: "t3".into(),
                name: "bash".into(),
                rule: "ls*".into(),
            },
            EngineEvent::ToolFlagged {
                id: "t4".into(),
                name: "write".into(),
                summary: "write Makefile".into(),
                why: "protected write".into(),
                denied: false,
            },
            EngineEvent::ToolFlagged {
                id: "t5".into(),
                name: "write".into(),
                summary: "write /outside".into(),
                why: "outside-session write".into(),
                denied: true,
            },
            // Both `child_tool` phases; the done phase uses `ok: false`
            // because done-ok is a deliberate `None` (asserted below).
            EngineEvent::ChildTool {
                parent_id: "t1".into(),
                id: "c1".into(),
                name: "read".into(),
                summary: "read ./x".into(),
                ok: None,
                tokens: None,
            },
            EngineEvent::ChildTool {
                parent_id: "t1".into(),
                id: "c1".into(),
                name: "read".into(),
                summary: String::new(),
                ok: Some(false),
                tokens: None,
            },
            EngineEvent::Retrying {
                attempt: 1,
                max: 5,
                reason: "429".into(),
                delay_ms: 1_500,
                status: Some(429),
                request_id: None,
                scope: hotl_engine::RetryScope::Sample,
                discarded_partial: false,
            },
            EngineEvent::FallbackModel { model: "m2".into() },
            EngineEvent::PromptQueued,
            EngineEvent::CacheWarmed {
                usage: hotl_types::TokenUsage::default(),
                cost_usd: Some(0.03),
            },
            EngineEvent::Compacted {
                degraded: false,
                focus: None,
                usage: hotl_types::TokenUsage::default(),
                estimate: 0,
            },
            EngineEvent::FoldSkipped {
                reason: "nothing".into(),
            },
            EngineEvent::ModelChanged {
                model: "openai/gpt-5".into(),
                previous: "m".into(),
                context_tokens: 12,
                context_window: 200_000,
            },
            EngineEvent::TodosChanged { items: Vec::new() },
            EngineEvent::GoalChanged {
                condition: Some("tests pass".into()),
            },
            EngineEvent::GoalChanged { condition: None },
            EngineEvent::GoalVerdict {
                verdict: hotl_engine::GoalVerdictKind::NotYet,
                reason: "no commit yet".into(),
                turns: 1,
                usage: hotl_types::TokenUsage::default(),
                evidence: Vec::new(),
            },
        ]
    }

    /// §7: attach rendered 4 of 13 variants, so an attached human silently
    /// missed denials, auto-allow rules, retries, fallbacks, queued prompts,
    /// todos, and thinking. Rendering *from* the shared frame means a new
    /// variant can never be dropped on this surface alone again.
    #[test]
    fn attach_renders_every_update_variant() {
        // The two deliberate `None`s, each for a stated reason: `text_delta`
        // is the answer itself (streamed to stdout unadorned), and a tool that
        // *succeeded* needs no line — its output already spoke.
        // `tool_progress` is exempt by decision 15: a tail row is a console
        // affordance, and a line printer cannot show one without spamming;
        // `tool_input_progress` (0062 T3) likewise is a moving byte count.
        let exempt = [
            "text_delta",
            "tool_done",
            "tool_progress",
            "tool_input_progress",
        ];
        for e in every_streamable_event() {
            let Some(frame) = crate::wire::update_frame(&e) else {
                continue;
            };
            if exempt.contains(&frame["type"].as_str().unwrap_or("")) {
                continue;
            }
            assert!(
                update_line(&frame).is_some(),
                "attach drops `{}`",
                frame["type"]
            );
        }
        // A tool that failed does get a line — the exemption is about success.
        let failed = crate::wire::update_frame(&EngineEvent::ToolDone {
            id: "t1".into(),
            name: "read".into(),
            ok: false,
            lines: 1,
            bytes: 4,
            output: String::new(),
            before: None,
        })
        .unwrap();
        assert!(update_line(&failed).is_some(), "a tool error must be shown");
    }

    /// 0039: `child_tool` mirrors `tool_done`'s success exemption per phase —
    /// something the blanket exempt list above cannot express.
    #[test]
    fn child_tool_lines_follow_the_tool_done_success_exemption() {
        let frame = |summary: &str, ok: Option<bool>| {
            crate::wire::update_frame(&EngineEvent::ChildTool {
                parent_id: "t1".into(),
                id: "c1".into(),
                name: "read".into(),
                summary: summary.into(),
                ok,
                tokens: None,
            })
            .unwrap()
        };
        assert_eq!(
            update_line(&frame("read ./x", None)).as_deref(),
            Some("  ↳ read ./x")
        );
        assert!(
            update_line(&frame("", Some(true))).is_none(),
            "a child that worked needs no line"
        );
        assert_eq!(
            update_line(&frame("", Some(false))).as_deref(),
            Some("  ↳ (sub-agent tool error)")
        );
    }

    #[test]
    fn typed_lines_route_correctly() {
        let ask = Parked {
            id: 4,
            egress: None,
            prompt: String::new(),
        };
        assert_eq!(route("/stop", false, None), Action::Stop);
        assert_eq!(
            route("y", false, Some(&ask)),
            Action::AnswerAsk {
                parked: ask.clone(),
                allow: true
            }
        );
        assert_eq!(
            route("no", false, Some(&ask)),
            Action::AnswerAsk {
                parked: ask.clone(),
                allow: false
            }
        );
        // An egress ask routes the same way but answers a different method —
        // the kind travels with the id so it can never be mis-routed.
        let egress = Parked {
            id: 5,
            egress: Some("registry.npmjs.org".into()),
            prompt: String::new(),
        };
        assert_eq!(
            route("y", false, Some(&egress)),
            Action::AnswerAsk {
                parked: egress,
                allow: true
            }
        );
        assert_eq!(route("", false, None), Action::Nothing);
        assert_eq!(route("hi", true, None), Action::Steer("hi".into()));
        assert_eq!(route("hi", false, None), Action::Prompt("hi".into()));
    }

    #[test]
    fn a_bang_line_runs_locally_and_is_never_a_prompt() {
        assert_eq!(
            route("!git status", false, None),
            Action::Shell("git status".into())
        );
        assert_eq!(
            route("! ls -la ", true, None),
            Action::Shell("ls -la".into())
        );
        assert_eq!(route("!", false, None), Action::ShellUsage);
        assert_eq!(route("!   ", true, None), Action::ShellUsage);
    }

    #[test]
    fn a_bang_line_with_an_ask_parked_leaves_the_ask_parked() {
        let ask = Parked {
            id: 4,
            egress: None,
            prompt: "allow x? [y/N] ".into(),
        };
        assert_eq!(route("!ls", false, Some(&ask)), Action::Shell("ls".into()));
        assert_eq!(route("!", true, Some(&ask)), Action::ShellUsage);
        assert!(matches!(
            route("y", false, Some(&ask)),
            Action::AnswerAsk { allow: true, .. }
        ));
    }

    #[test]
    fn a_shell_result_starts_on_a_fresh_line() {
        assert_eq!(shell_text("ls", "a\nb", false), "$ ls\na\nb\n");
        assert_eq!(shell_text("true", "", true), "\n$ true\n");
    }

    #[tokio::test]
    async fn session_output_prints_while_a_bang_runs() {
        let (shell_tx, mut shell_rx) = mpsc::unbounded_channel::<ShellDone>();
        let (server_tx, mut server_rx) = mpsc::unbounded_channel::<Value>();
        spawn_bang(
            "sleep 1; echo late".into(),
            std::path::PathBuf::from("."),
            shell_tx,
        );
        server_tx
            .send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "usage_update"}}}))
            .unwrap();
        let mut order = Vec::new();
        for _ in 0..2 {
            tokio::select! {
                Some(_) = server_rx.recv() => order.push("server"),
                Some(done) = shell_rx.recv() => {
                    assert!(done.1.contains("late"), "{}", done.1);
                    order.push("shell");
                }
            }
        }
        assert_eq!(order, vec!["server", "shell"]);
    }

    #[test]
    fn a_bang_run_prints_its_output_and_builds_the_note() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker-0084-attach"), "").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (shown, params) = rt.block_on(bang("ls; exit 3", dir.path()));
        rt.shutdown_background();
        assert!(shown.contains("marker-0084-attach"), "{shown}");
        assert!(shown.ends_with("[exit 3]"), "{shown}");
        assert_eq!(params["command"], "ls; exit 3");
        assert_eq!(params["exitCode"], 3);
        assert_eq!(params["end"], "exited");
        assert!(
            params["output"]
                .as_str()
                .unwrap()
                .contains("marker-0084-attach"),
            "{params}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_killed_bang_run_shares_the_signal() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (shown, params) =
            rt.block_on(bang("echo hi; kill -TERM $$", std::path::Path::new(".")));
        rt.shutdown_background();
        assert!(shown.ends_with("[killed by SIGTERM]"), "{shown}");
        assert!(
            params["output"]
                .as_str()
                .unwrap()
                .ends_with("[killed by SIGTERM]"),
            "{params}"
        );
        assert!(params["exitCode"].is_null(), "{params}");
        assert_eq!(params["end"], "signaled");
    }

    fn usage_update() -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s",
            "update": {"sessionUpdate": "usage_update", "used": 1, "size": 2}}})
    }

    #[test]
    fn a_turn_someone_else_started_ends_on_its_usage_update() {
        let mut state = Attached::default();
        state.on_server(&json!({"jsonrpc": "2.0", "method": "_hotl/session/update",
            "params": {"sessionId": "s", "update": {"type": "prompt_queued"}}}));
        assert!(state.turn_running);
        state.on_server(&usage_update());
        assert!(
            !state.turn_running,
            "a typed line is a prompt again, not a steer"
        );
    }

    #[test]
    fn our_prompt_ends_on_its_reply_and_a_steer_ack_changes_nothing() {
        let mut state = Attached {
            prompts: vec![3],
            turn_running: true,
            ..Attached::default()
        };
        state.on_server(&usage_update());
        assert!(
            state.turn_running,
            "the reply, not the usage, ends our turn"
        );
        state.on_server(&json!({"jsonrpc": "2.0", "id": 4, "result": {"queued": true}}));
        assert!(state.turn_running);
        state.on_server(&json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}}));
        assert!(!state.turn_running);
        assert!(state.prompts.is_empty());
    }

    #[test]
    fn the_turn_end_kind_prefers_the_hotl_outcome() {
        assert_eq!(
            turn_end_kind(&Ok(json!({"stopReason": "end_turn",
                "_meta": {"hotl.dev/turn": {"outcome": {"kind": "doom_loop"}}}}))),
            "doom_loop"
        );
        assert_eq!(
            turn_end_kind(&Ok(json!({"stopReason": "cancelled"}))),
            "cancelled"
        );
        assert_eq!(turn_end_kind(&Err("boom".into())), "error");
    }

    #[test]
    fn a_server_notice_gets_a_line() {
        assert_eq!(
            update_line(&json!({"type": "server_notice", "text": "1 parked ask(s) expired"}))
                .as_deref(),
            Some("· 1 parked ask(s) expired")
        );
    }

    #[tokio::test]
    async fn attach_json_prints_one_object_per_live_session() {
        use crate::run_status::{RunState, RunStatus};
        let ids: Vec<String> = ["a", "b"]
            .iter()
            .map(|tag| format!("aj-{tag}-{}", std::process::id()))
            .collect();
        let statuses: Vec<RunStatus> = ids
            .iter()
            .zip([RunState::Idle, RunState::Waiting])
            .map(|(id, state)| RunStatus {
                run_id: id.clone(),
                session_id: format!("stored-{id}"),
                pid: 7,
                cwd: std::env::temp_dir(),
                name: Some(format!("name of {id}")),
                version: "1.2.3".into(),
                started_at_ms: 99,
                state,
            })
            .collect();
        let listeners: Vec<_> = ids
            .iter()
            .map(|id| hotl_platform::IPC.bind_private(id).unwrap())
            .collect();
        for status in &statuses {
            crate::run_status::write(status).unwrap();
        }
        let lines = live_json();
        for id in &ids {
            crate::run_status::remove(id);
            if let Some(p) = hotl_platform::IPC.artifact_path(id) {
                let _ = std::fs::remove_file(p);
            }
        }
        drop(listeners);
        let ours: Vec<RunStatus> = lines
            .iter()
            .filter_map(|line| serde_json::from_str::<RunStatus>(line).ok())
            .filter(|s| ids.contains(&s.run_id))
            .collect();
        assert_eq!(ours, statuses, "{lines:?}");
        assert!(lines.iter().all(|line| !line.contains('\n')), "{lines:?}");
    }
}
