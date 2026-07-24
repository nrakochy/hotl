//! Bare `hotl` — terminal runtime for the execute console. All decisions live
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
use hotl_theme::Palette;
use hotl_tui::app::{update, Cmd, Msg, Phase, State};
use hotl_tui::client::{read_server_msg, AcpClient, ServerMsg};
use hotl_tui::view::view;
use ratatui::prelude::*;
use serde_json::{json, Value};
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

type ServerReader = BufReader<ReadHalf<DuplexStream>>;
type Client = AcpClient<WriteHalf<DuplexStream>>;

pub async fn tui_main(args: Vec<String>) -> i32 {
    use std::io::IsTerminal;
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        eprintln!(
            "hotl: the console TUI needs a terminal — use `hotl -p \"prompt\"` for scripted runs"
        );
        return 2;
    }
    let TuiArgs { spec, name } = match parse_tui_args(&args) {
        Ok(a) => a,
        Err(code) => return code,
    };
    let (factory, model, skills) = match crate::agent::acp_factory().await {
        Ok(triple) => triple,
        Err(code) => return code,
    };
    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    let vim_mode = cfg.behavior.vim_mode.unwrap_or(true);
    // Prompt-history tail, loaded (and startup-compacted) before the screen is
    // taken; the store is handed to the loop to append each submitted prompt.
    let (history_store, history) =
        crate::history::History::load(&cfg.history, &crate::agent::data_dir());
    if let Some(hint) = crate::setup::first_run_hint(&crate::agent::config_dir()) {
        eprintln!("hotl: {hint}");
    }
    // Same [settings.theme] table (and warning behavior) as `hotl watch`;
    // warnings print as plain lines before the alternate screen owns stdout.
    let (watch_cfg, theme_warn) = watch_types::HotlConfig::load_with_warning();
    if let Some(w) = theme_warn {
        eprintln!("hotl: {w}");
    }
    let palette = Palette::from(&watch_cfg.settings.theme.resolve().0);
    let (density, density_warn) = watch_cfg.settings.density();
    if let Some(w) = density_warn {
        eprintln!("hotl: {w}");
    }

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server_io);
    tokio::spawn(crate::acp::serve(sread, swrite, factory, skills));
    let (cread, cwrite) = tokio::io::split(client_io);
    let mut reader = BufReader::new(cread);
    let mut client = AcpClient::new(cwrite);

    let (session_name, skills) = match handshake(&mut client, &mut reader, spec, name).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("hotl: {e}");
            return 1;
        }
    };

    let suspended = Arc::new(AtomicBool::new(false));
    let keys = spawn_key_reader(suspended.clone());
    // Armed before the screen is taken: a signal or panic between here and
    // the guard's `Drop` must not strand the terminal in raw mode.
    crate::term::restore_on_panic();
    crate::term::trap_signals();
    let mut guard = match TerminalGuard::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("hotl: {e}");
            return 1;
        }
    };
    let mut state = State::new(vim_mode, model);
    state.session_name = session_name;
    state.skills = skills;
    state.density = density;
    state.editor.load_history(history);
    let result = run_loop(
        &mut guard,
        &mut client,
        &mut reader,
        keys,
        &suspended,
        state,
        palette,
        history_store,
    )
    .await;
    drop(guard);
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hotl: {e}");
            1
        }
    }
}

/// initialize + session/new|load before entering raw mode, so wiring errors
/// print as plain lines instead of corrupting an alt-screen. Returns the
/// opened session's display name (server-confirmed) and the skill names
/// `initialize` advertised, which is what makes `/<skill>` resolvable.
async fn handshake(
    client: &mut Client,
    reader: &mut ServerReader,
    spec: Option<String>,
    name: Option<String>,
) -> Result<(Option<String>, Vec<String>), String> {
    let init = client.request("initialize", Value::Null).await;
    let hello = wait_response(reader, init).await?;
    let skills: Vec<String> = hello
        .get("skills")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let open = match spec {
        None => client.request("session/new", json!({"name": name})).await,
        Some(sid) => {
            client
                .request("session/load", json!({"sessionId": sid, "name": name}))
                .await
        }
    };
    let v = wait_response(reader, open).await?;
    Ok((
        v.get("name").and_then(Value::as_str).map(String::from),
        skills,
    ))
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

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    guard: &mut TerminalGuard,
    client: &mut Client,
    reader: &mut ServerReader,
    mut keys: mpsc::Receiver<Event>,
    suspended: &AtomicBool,
    mut state: State,
    palette: Palette,
    mut history: crate::history::History,
) -> io::Result<i32> {
    let mut prompt_ids: VecDeque<u64> = VecDeque::new();
    // 8 ticks/sec, armed only while a turn runs — idle schedules no wakeups.
    let mut ticker = tokio::time::interval(Duration::from_millis(125));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        guard.terminal.draw(|f| view(&state, &palette, f))?;
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
                    prompt_ids.push_back(
                        client
                            .request("session/prompt", json!({"text": text}))
                            .await,
                    );
                }
                Cmd::SendSteer(text) => {
                    client.request("session/steer", json!({"text": text})).await;
                }
                Cmd::Cancel => {
                    client.request("session/cancel", Value::Null).await;
                }
                Cmd::Rename(name) => {
                    // Ack is noise (like steer): translate() only surfaces
                    // prompt-id responses.
                    client
                        .request("session/rename", json!({"name": name}))
                        .await;
                }
                Cmd::SetMode(mode) => {
                    // Ack is noise, same as rename/steer.
                    client
                        .request("session/set_mode", json!({"mode": mode}))
                        .await;
                }
                Cmd::ReplyPermission {
                    req_id,
                    allow,
                    message,
                } => client.reply_permission(req_id, allow, message).await,
                Cmd::SetTitle(title) => {
                    let _ = execute!(io::stdout(), SetTitle(&title));
                }
                Cmd::AppendHistory(text) => history.append(&text),
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
        ServerMsg::PermissionRequest {
            req_id,
            summary,
            protected_why,
        } => Some(Msg::PermissionRequest {
            req_id,
            summary,
            protected_why,
        }),
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
                outcome_kind: v
                    .pointer("/outcome/kind")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string(),
                outcome_text: text,
                usage: v.get("usage").cloned().unwrap_or(Value::Null),
            }
        }
        Err(e) => Msg::PromptResult {
            outcome_kind: "error".into(),
            outcome_text: Some(e),
            usage: Value::Null,
        },
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

fn suspended_editor(
    guard: &mut TerminalGuard,
    suspended: &AtomicBool,
    text: &str,
) -> Option<String> {
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

#[derive(Debug)]
pub(crate) struct TuiArgs {
    pub spec: Option<String>,
    pub name: Option<String>,
}

/// The console's argument surface: `[id-prefix]`, `-r/--resume [arg]`,
/// `-n/--name <name>`. `hotl resume [arg]` arrives here rewritten to
/// `--resume` by main.rs.
fn parse_tui_args(args: &[String]) -> Result<TuiArgs, i32> {
    let mut spec: Option<String> = None;
    let mut resume_bare = false;
    let mut name: Option<String> = None;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            // The one-time migration hint: `tui` was a subcommand before the
            // default flip, and would otherwise read as a session-id prefix.
            "tui" => {
                eprintln!("hotl: the TUI is now just `hotl` (the `tui` subcommand was removed)");
                return Err(2);
            }
            "-r" | "--resume" => match it.peek() {
                Some(v) if !v.starts_with('-') => {
                    let arg = it.next().expect("peeked");
                    spec = Some(resolve_session_arg(arg, &newest_first())?);
                }
                _ => resume_bare = true,
            },
            "-n" | "--name" => {
                match it
                    .next()
                    .map(String::as_str)
                    .and_then(hotl_types::normalize_session_name)
                {
                    Some(n) => name = Some(n),
                    None => {
                        eprintln!("hotl: -n/--name needs a value of 1–64 chars");
                        return Err(2);
                    }
                }
            }
            flag if flag.starts_with('-') => {
                eprintln!("hotl: unknown argument `{flag}` (try --help)");
                return Err(2);
            }
            prefix => spec = Some(by_prefix(prefix)?),
        }
    }
    if resume_bare && spec.is_none() {
        spec = Some(pick_session()?);
    }
    Ok(TuiArgs { spec, name })
}

/// `-r <arg>` resolution: picker list number → unique id-prefix → unique
/// exact name. Ambiguity errors; it never falls through past a hit.
fn resolve_session_arg(
    arg: &str,
    sessions: &[(String, PathBuf, SystemTime)],
) -> Result<String, i32> {
    if let Ok(n) = arg.parse::<usize>() {
        if (1..=sessions.len().min(20)).contains(&n) {
            return Ok(sessions[n - 1].0.clone());
        }
    }
    let by_id: Vec<_> = sessions
        .iter()
        .filter(|(id, ..)| id.starts_with(arg))
        .collect();
    match by_id.len() {
        1 => return Ok(by_id[0].0.clone()),
        0 => {}
        n => {
            eprintln!("hotl: `{arg}` is ambiguous ({n} sessions)");
            return Err(2);
        }
    }
    let by_name: Vec<_> = sessions
        .iter()
        .filter(|(_, path, _)| hotl_store::session_name(path).as_deref() == Some(arg))
        .collect();
    match by_name.len() {
        1 => Ok(by_name[0].0.clone()),
        0 => {
            eprintln!("hotl: no session matches `{arg}`");
            Err(2)
        }
        n => {
            let ids: Vec<&str> = by_name.iter().map(|(id, ..)| id.as_str()).collect();
            eprintln!(
                "hotl: {n} sessions are named `{arg}` — use the id: {}",
                ids.join(", ")
            );
            Err(2)
        }
    }
}

fn by_prefix(prefix: &str) -> Result<String, i32> {
    let sessions = newest_first();
    let matches: Vec<_> = sessions
        .iter()
        .filter(|(id, ..)| id.starts_with(prefix))
        .collect();
    match matches.len() {
        1 => Ok(matches[0].0.clone()),
        0 => {
            eprintln!("hotl: no session matches `{prefix}`");
            Err(2)
        }
        n => {
            eprintln!("hotl: `{prefix}` is ambiguous ({n} sessions)");
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
        eprintln!("hotl: no sessions to resume");
        return Err(2);
    }
    eprintln!("pick a session:");
    for (i, (id, path, t)) in sessions.iter().enumerate().take(20) {
        match hotl_store::session_name(path) {
            Some(name) => eprintln!("  {}) {id}  {name}  {}", i + 1, age(*t)),
            None => eprintln!("  {}) {id}  {}", i + 1, age(*t)),
        }
    }
    eprint!("> ");
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return Err(2);
    }
    match line.trim().parse::<usize>() {
        Ok(n) if (1..=sessions.len().min(20)).contains(&n) => Ok(sessions[n - 1].0.clone()),
        _ => {
            eprintln!("hotl: not a valid choice");
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
        crate::term::capture();
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => {
                crate::term::arm();
                Ok(TerminalGuard { terminal })
            }
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
        crate::term::disarm();
    }

    /// …and take it back.
    fn resume(&mut self) {
        let _ = enable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), EnterAlternateScreen);
        let _ = self.terminal.clear();
        crate::term::arm();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
        crate::term::disarm();
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_tui_args, resolve_session_arg};
    use std::time::SystemTime;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// Synthetic newest-first session list; only the picked id matters.
    fn sessions(ids: &[&str]) -> Vec<(String, std::path::PathBuf, SystemTime)> {
        ids.iter()
            .map(|id| {
                (
                    id.to_string(),
                    std::path::PathBuf::from(format!("/nonexistent/{id}.jsonl")),
                    SystemTime::now(),
                )
            })
            .collect()
    }

    #[test]
    fn bare_args_open_a_new_unnamed_session() {
        let args = parse_tui_args(&v(&[])).unwrap();
        assert_eq!(args.spec, None);
        assert_eq!(args.name, None);
    }

    #[test]
    fn tui_literal_gets_the_migration_hint() {
        // Pre-flip muscle memory: `hotl tui` must not read as an id prefix.
        assert_eq!(parse_tui_args(&v(&["tui"])).unwrap_err(), 2);
    }

    #[test]
    fn unknown_flags_are_rejected_before_session_lookup() {
        assert_eq!(parse_tui_args(&v(&["--json"])).unwrap_err(), 2);
        assert_eq!(parse_tui_args(&v(&["-x"])).unwrap_err(), 2);
    }

    #[test]
    fn name_flag_is_normalized_and_validated() {
        let args = parse_tui_args(&v(&["-n", "  fix-auth  "])).unwrap();
        assert_eq!(args.name.as_deref(), Some("fix-auth"));
        assert_eq!(parse_tui_args(&v(&["-n"])).unwrap_err(), 2);
        assert_eq!(parse_tui_args(&v(&["--name", "   "])).unwrap_err(), 2);
    }

    #[test]
    fn list_number_beats_prefix() {
        // "2" would also be a valid id-prefix here; the picker number wins.
        let s = sessions(&["01AAA", "2ZZZZ", "01BBB"]);
        assert_eq!(resolve_session_arg("2", &s), Ok("2ZZZZ".to_string()));
    }

    #[test]
    fn out_of_range_number_falls_through_to_prefix() {
        let s = sessions(&["01AAA", "01BBB"]);
        assert_eq!(resolve_session_arg("01A", &s), Ok("01AAA".to_string()));
        assert_eq!(resolve_session_arg("9", &s).unwrap_err(), 2);
    }

    #[test]
    fn ambiguous_prefix_is_an_error() {
        let s = sessions(&["01AAA", "01ABB"]);
        assert_eq!(resolve_session_arg("01A", &s).unwrap_err(), 2);
    }

    #[test]
    fn unmatched_arg_reports_no_session() {
        let s = sessions(&["01AAA"]);
        assert_eq!(resolve_session_arg("zzz", &s).unwrap_err(), 2);
    }

    #[test]
    fn name_resolution_reads_the_log() {
        // Real logs on disk so session_name() finds the rename entry.
        let dir = tempfile::tempdir().unwrap();
        let mut named =
            hotl_store::SessionLog::create(dir.path(), "m", None, hotl_store::Masker::empty(), 1)
                .unwrap();
        named
            .append(
                &hotl_types::EntryPayload::Rename {
                    name: "fix-auth".into(),
                },
                2,
            )
            .unwrap();
        let plain =
            hotl_store::SessionLog::create(dir.path(), "m", None, hotl_store::Masker::empty(), 3)
                .unwrap();
        let s = vec![
            (
                named.session_id.clone(),
                named.path().to_path_buf(),
                SystemTime::now(),
            ),
            (
                plain.session_id.clone(),
                plain.path().to_path_buf(),
                SystemTime::now(),
            ),
        ];
        assert_eq!(
            resolve_session_arg("fix-auth", &s),
            Ok(named.session_id.clone())
        );
    }
}
