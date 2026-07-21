//! `hotl tui` — terminal runtime for the execute console. All decisions live
//! in `hotl-tui` (pure Elm core); this file owns the I/O: raw mode, the event
//! loop, `$EDITOR` suspension, and the in-process duplex to `acp::serve`. The
//! TUI is a pure ACP client — it never touches the engine directly.

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crossterm::event::{Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use hotl_tui::app::{update, Cmd, Msg, Phase, State};
use hotl_tui::client::{read_server_msg, AcpClient, ServerMsg};
use hotl_tui::view::{view, Theme};
use ratatui::prelude::*;
use serde_json::{json, Value};
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

type ServerReader = BufReader<ReadHalf<DuplexStream>>;
type Client = AcpClient<WriteHalf<DuplexStream>>;

pub async fn tui_main(args: Vec<String>) -> i32 {
    let spec = match resolve_spec(&args) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let (factory, model) = match crate::agent::acp_factory().await {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    let vim_mode = crate::config::Config::load(&crate::agent::config_dir()).behavior.vim_mode.unwrap_or(true);

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server_io);
    tokio::spawn(crate::acp::serve(sread, swrite, factory));
    let (cread, cwrite) = tokio::io::split(client_io);
    let mut reader = BufReader::new(cread);
    let mut client = AcpClient::new(cwrite);

    if let Err(e) = handshake(&mut client, &mut reader, spec).await {
        eprintln!("hotl tui: {e}");
        return 1;
    }

    let suspended = Arc::new(AtomicBool::new(false));
    let keys = spawn_key_reader(suspended.clone());
    let mut guard = match TerminalGuard::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("hotl tui: {e}");
            return 1;
        }
    };
    let state = State::new(vim_mode, model);
    let result = run_loop(&mut guard, &mut client, &mut reader, keys, &suspended, state).await;
    drop(guard);
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hotl tui: {e}");
            1
        }
    }
}

/// initialize + session/new|load before entering raw mode, so wiring errors
/// print as plain lines instead of corrupting an alt-screen.
async fn handshake(client: &mut Client, reader: &mut ServerReader, spec: Option<String>) -> Result<(), String> {
    let init = client.request("initialize", Value::Null).await;
    wait_response(reader, init).await?;
    let open = match spec {
        None => client.request("session/new", Value::Null).await,
        Some(sid) => client.request("session/load", json!({"sessionId": sid})).await,
    };
    wait_response(reader, open).await?;
    Ok(())
}

async fn wait_response(reader: &mut ServerReader, want: u64) -> Result<Value, String> {
    loop {
        match read_server_msg(reader).await {
            None => return Err("server closed during handshake".into()),
            Some(ServerMsg::Response { id, result }) if id == want => return result,
            Some(_) => {}
        }
    }
}

async fn run_loop(
    guard: &mut TerminalGuard,
    client: &mut Client,
    reader: &mut ServerReader,
    mut keys: mpsc::Receiver<Event>,
    suspended: &AtomicBool,
    mut state: State,
) -> io::Result<i32> {
    let theme = Theme::default();
    let mut prompt_ids: VecDeque<u64> = VecDeque::new();
    // 8 ticks/sec, armed only while a turn runs — idle schedules no wakeups.
    let mut ticker = tokio::time::interval(Duration::from_millis(125));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        guard.terminal.draw(|f| view(&state, &theme, f))?;
        let msg = tokio::select! {
            ev = keys.recv() => match ev {
                Some(Event::Key(k)) if k.kind == KeyEventKind::Press => Some(Msg::Key(k)),
                Some(_) => None, // resize etc: redraw happens on loop
                None => return Ok(1),
            },
            sm = read_server_msg(reader) => match sm {
                Some(m) => translate(m, &mut prompt_ids),
                None => return Ok(1), // server hung up
            },
            _ = ticker.tick(), if state.phase != Phase::Idle => Some(Msg::Tick),
        };
        let Some(msg) = msg else { continue };
        let mut queue: VecDeque<Cmd> = update(&mut state, msg).into();
        while let Some(cmd) = queue.pop_front() {
            match cmd {
                Cmd::SendPrompt(text) => {
                    prompt_ids.push_back(client.request("session/prompt", json!({"text": text})).await);
                }
                Cmd::SendSteer(text) => {
                    client.request("session/steer", json!({"text": text})).await;
                }
                Cmd::Cancel => {
                    client.request("session/cancel", Value::Null).await;
                }
                Cmd::ReplyPermission { req_id, allow, message } => client.reply_permission(req_id, allow, message).await,
                Cmd::SetTitle(title) => {
                    let _ = execute!(io::stdout(), SetTitle(&title));
                }
                Cmd::OpenEditor(text) => {
                    let content = suspended_editor(guard, suspended, &text);
                    queue.extend(update(&mut state, Msg::EditorDone(content)));
                }
                Cmd::Quit => return Ok(0),
            }
        }
    }
}

fn translate(msg: ServerMsg, prompt_ids: &mut VecDeque<u64>) -> Option<Msg> {
    match msg {
        ServerMsg::Update(v) => Some(Msg::Update(v)),
        ServerMsg::PermissionRequest { req_id, summary, protected_why } => {
            Some(Msg::PermissionRequest { req_id, summary, protected_why })
        }
        ServerMsg::Response { id, result } => {
            // Only prompt replies become messages; steer/cancel acks are noise.
            let pos = prompt_ids.iter().position(|&p| p == id)?;
            prompt_ids.remove(pos);
            Some(prompt_result_msg(result))
        }
    }
}

fn prompt_result_msg(result: Result<Value, String>) -> Msg {
    match result {
        Ok(v) => {
            let text = ["text", "message", "pattern", "tool"]
                .iter()
                .find_map(|k| v.pointer(&format!("/outcome/{k}")).and_then(Value::as_str))
                .map(String::from);
            Msg::PromptResult {
                outcome_kind: v.pointer("/outcome/kind").and_then(Value::as_str).unwrap_or("error").to_string(),
                outcome_text: text,
                usage: v.get("usage").cloned().unwrap_or(Value::Null),
            }
        }
        Err(e) => Msg::PromptResult { outcome_kind: "error".into(), outcome_text: Some(e), usage: Value::Null },
    }
}

/// Crossterm events read on a plain thread (no event-stream feature needed);
/// `suspended` parks it while `$EDITOR` owns the terminal.
fn spawn_key_reader(suspended: Arc<AtomicBool>) -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel(64);
    std::thread::spawn(move || loop {
        if suspended.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(true) => match crossterm::event::read() {
                Ok(ev) => {
                    if tx.blocking_send(ev).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            Ok(false) => {}
            Err(_) => return,
        }
    });
    rx
}

fn suspended_editor(guard: &mut TerminalGuard, suspended: &AtomicBool, text: &str) -> Option<String> {
    suspended.store(true, Ordering::Relaxed);
    guard.suspend();
    let content = run_external_editor(text);
    guard.resume();
    suspended.store(false, Ordering::Relaxed);
    content
}

/// Blocking is fine — the TUI is suspended. `None` = unchanged or aborted.
fn run_external_editor(text: &str) -> Option<String> {
    let path = std::env::temp_dir().join(format!("hotl-tui-{}.md", std::process::id()));
    std::fs::write(&path, text).ok()?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} '{}'", path.display()))
        .status();
    let content = match status {
        Ok(s) if s.success() => std::fs::read_to_string(&path).ok(),
        _ => None,
    };
    let _ = std::fs::remove_file(&path);
    content.filter(|c| c.trim_end() != text.trim_end())
}

fn resolve_spec(args: &[String]) -> Result<Option<String>, i32> {
    match args.first().map(String::as_str) {
        None => Ok(None),
        Some("--resume") => match args.get(1) {
            Some(p) => by_prefix(p).map(Some),
            None => pick_session().map(Some),
        },
        Some(prefix) => by_prefix(prefix).map(Some),
    }
}

fn by_prefix(prefix: &str) -> Result<String, i32> {
    let sessions = newest_first();
    let matches: Vec<_> = sessions.iter().filter(|(id, ..)| id.starts_with(prefix)).collect();
    match matches.len() {
        1 => Ok(matches[0].0.clone()),
        0 => {
            eprintln!("hotl tui: no session matches `{prefix}`");
            Err(2)
        }
        n => {
            eprintln!("hotl tui: `{prefix}` is ambiguous ({n} sessions)");
            Err(2)
        }
    }
}

fn newest_first() -> Vec<(String, PathBuf, SystemTime)> {
    let mut sessions = hotl_store::list_sessions(&crate::agent::sessions_dir());
    sessions.sort_by_key(|s| std::cmp::Reverse(s.2));
    sessions
}

/// Plain pre-TUI list prompt (the in-TUI picker is a v1 cut).
fn pick_session() -> Result<String, i32> {
    let sessions = newest_first();
    if sessions.is_empty() {
        eprintln!("hotl tui: no sessions to resume");
        return Err(2);
    }
    eprintln!("pick a session:");
    for (i, (id, _, t)) in sessions.iter().enumerate().take(20) {
        eprintln!("  {}) {id}  {}", i + 1, age(*t));
    }
    eprint!("> ");
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return Err(2);
    }
    match line.trim().parse::<usize>() {
        Ok(n) if (1..=sessions.len().min(20)).contains(&n) => Ok(sessions[n - 1].0.clone()),
        _ => {
            eprintln!("hotl tui: not a valid choice");
            Err(2)
        }
    }
}

fn age(t: SystemTime) -> String {
    let secs = t.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

/// Owns raw mode + alt screen, restoring on drop (mirrors watch.rs) — an
/// early error, normal exit, or panic all leave the shell usable.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(TerminalGuard { terminal }),
            Err(e) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(e)
            }
        }
    }

    /// Hand the real screen to `$EDITOR`…
    fn suspend(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
    }

    /// …and take it back.
    fn resume(&mut self) {
        let _ = enable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), EnterAlternateScreen);
        let _ = self.terminal.clear();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}
