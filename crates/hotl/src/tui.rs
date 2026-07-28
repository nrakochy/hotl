//! Bare `hotl` — terminal runtime for the execute console. All decisions live
//! in `hotl-tui` (pure Elm core); this file owns the I/O: raw mode, the event
//! loop, `$EDITOR` suspension, and the in-process duplex to `acp::serve`. The
//! TUI is a pure ACP client — it never touches the engine directly.

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crossterm::event::{Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use hotl_theme::Palette;
use hotl_tui::app::{update, Cmd, Msg, Phase, State};
use hotl_tui::client::{exec_wire_cmd, read_server_msg, translate, AcpClient, ServerMsg};
use hotl_tui::view::view;

use crate::term::TerminalGuard;
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
    let (factory, model, info) = match crate::agent::acp_factory().await {
        Ok(triple) => triple,
        Err(code) => return code,
    };
    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    // Prompt-history tail, loaded (and startup-compacted) before the screen is
    // taken; the store is handed to the loop to append each submitted prompt.
    // Startup-only on purpose — `/reload` does not re-read `[history]`.
    let (history_store, history) =
        crate::history::History::load(&cfg.history, &crate::agent::data_dir());
    if let Some(hint) = crate::setup::first_run_hint(&crate::agent::config_dir()) {
        eprintln!("hotl: {hint}");
    }
    // The same read `/reload` repeats. Warnings print as plain lines here,
    // before the alternate screen owns stdout; on reload they become notices.
    let settings = client_settings();
    for w in &settings.warnings {
        eprintln!("hotl: {w}");
    }

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server_io);
    // `serve_with_reload`, not `serve`: `/reload` asks the engine to rebuild
    // itself from `config.toml`. The TUI stays a pure ACP client — it hands
    // over the hook and never calls it.
    tokio::spawn(crate::acp::serve(
        sread,
        swrite,
        factory,
        info,
        Some(crate::agent::reload_hook()),
    ));
    let (cread, cwrite) = tokio::io::split(client_io);
    let mut reader = BufReader::new(cread);
    let mut client = AcpClient::new(cwrite);

    let opened = match handshake(&mut client, &mut reader, spec, name).await {
        Ok(o) => o,
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
    let mut guard = match TerminalGuard::enter(settings.mouse) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("hotl: {e}");
            return 1;
        }
    };
    let Opened {
        name: session_name,
        skills,
        mode,
        context_window,
    } = opened;
    let mut state = State::new(settings.vim_mode, model);
    state.session_name = session_name;
    // Server-side truth, seeded before the first draw: the badge must never
    // render a mode the session is not actually running (evaluation §5.7).
    state.mode = mode;
    state.context_window = context_window;
    state.set_skills(skills);
    state.density = settings.density;
    state.editor.load_history(history);
    let result = run_loop(
        &mut guard,
        &mut client,
        &mut reader,
        keys,
        &suspended,
        state,
        settings.palette,
        history_store,
        settings.copy_on_select,
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

/// Session settings from the handshake pair. The session's own `mode` wins
/// over the server default (a resumed session inherits and coerces its own);
/// both fall back so a newer client against an older server still runs — it
/// simply shows the library default instead of guessing.
fn open_settings(hello: &Value, opened: &Value) -> (String, u64) {
    let mode = opened
        .get("mode")
        .or_else(|| hello.get("defaultMode"))
        .and_then(Value::as_str)
        .unwrap_or("ask")
        .to_string();
    let window = hello
        .get("contextWindow")
        .and_then(Value::as_u64)
        .filter(|&w| w > 0)
        .unwrap_or(hotl_tui::app::DEFAULT_CONTEXT_WINDOW);
    (mode, window)
}

/// The half of `config.toml` no server knows about: everything the console
/// renders or reads keys with. Read at startup and again on every `/reload`,
/// through the one function below — startup and reload cannot drift.
///
/// `[history]` is deliberately absent: the recall ring is loaded once, before
/// the screen is taken, and re-reading it mid-session would silently discard
/// prompts submitted since.
struct ClientSettings {
    palette: Palette,
    density: hotl_theme::Density,
    vim_mode: bool,
    mouse: bool,
    copy_on_select: bool,
    /// Theme and density parse warnings, printed at startup and shown as
    /// transcript notices on reload.
    warnings: Vec<String>,
}

fn client_settings() -> ClientSettings {
    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    // Same [settings.theme] table (and warning behavior) as `hotl watch`.
    let (watch_cfg, theme_warn) = watch_types::HotlConfig::load_with_warning();
    let (density, density_warn) = watch_cfg.settings.density();
    // Mouse capture makes the wheel scroll the transcript and a drag select.
    // It costs the terminal's own drag-select, which `copy_on_select` gives
    // back from inside the app (hold Shift on most emulators for the real
    // thing, or turn capture off entirely).
    let mouse = resolve_mouse(
        std::env::var("HOTL_MOUSE").ok().as_deref(),
        cfg.behavior.mouse(),
    );
    ClientSettings {
        palette: Palette::from(&watch_cfg.settings.theme.resolve().0),
        density,
        vim_mode: cfg.behavior.vim_mode(),
        mouse,
        // Without capture there are no drag events at all, so the copy feature
        // depends on it rather than silently doing nothing.
        copy_on_select: mouse && cfg.behavior.copy_on_select(),
        warnings: theme_warn.into_iter().chain(density_warn).collect(),
    }
}

/// What the pre-TUI handshake learned: the opened session's display name
/// (server-confirmed), the skills `initialize` advertised (what makes
/// `/<skill>` resolvable), and the session's effective permission mode plus
/// context window — both server-side truth the client renders, never infers.
struct Opened {
    name: Option<String>,
    skills: Vec<(String, String)>,
    mode: String,
    context_window: u64,
}

/// initialize + session/new|load before entering raw mode, so wiring errors
/// print as plain lines instead of corrupting an alt-screen.
async fn handshake(
    client: &mut Client,
    reader: &mut ServerReader,
    spec: Option<String>,
    name: Option<String>,
) -> Result<Opened, String> {
    let init = client.request("initialize", Value::Null).await;
    let hello = wait_response(reader, init).await?;
    let skills = hotl_tui::client::parse_skills(&hello);
    let open = match spec {
        None => client.request("session/new", json!({"name": name})).await,
        Some(sid) => {
            client
                .request("session/load", json!({"sessionId": sid, "name": name}))
                .await
        }
    };
    let v = wait_response(reader, open).await?;
    let (mode, context_window) = open_settings(&hello, &v);
    Ok(Opened {
        name: v.get("name").and_then(Value::as_str).map(String::from),
        skills,
        mode,
        context_window,
    })
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
    mut palette: Palette,
    mut history: crate::history::History,
    mut copy_on_select: bool,
) -> io::Result<i32> {
    let mut prompt_ids: VecDeque<u64> = VecDeque::new();
    // 8 ticks/sec, armed only while a turn runs — idle schedules no wakeups.
    let mut ticker = tokio::time::interval(Duration::from_millis(125));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        guard.terminal.draw(|f| view(&state, &palette, f))?;
        let msg = tokio::select! {
            ev = keys.recv() => match ev {
                Some(ev) => terminal_msg(ev, copy_on_select), // `None` = redraw-only (resize, mouse motion)
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
            // The wire-bound half is shared with the e2e harness; what comes
            // back is the terminal-bound remainder this loop owns.
            // `@[file]` refs and image paths expand on the way out only.
            let cmd = match cmd {
                Cmd::SendPrompt(p) => Cmd::SendPrompt(outbound(p)),
                Cmd::SendSteer(p) => Cmd::SendSteer(outbound(p)),
                other => other,
            };
            let Some(cmd) = exec_wire_cmd(cmd, client, &mut prompt_ids).await else {
                continue;
            };
            match cmd {
                Cmd::SetTitle(title) => {
                    let _ = execute!(io::stdout(), SetTitle(&title));
                }
                Cmd::AppendHistory(text) => history.append(&text),
                Cmd::CopySelection(sel) => {
                    let lines = copy_region(guard, &state, &palette, &sel)?;
                    queue.extend(update(&mut state, Msg::Copied { lines }));
                }
                Cmd::OpenEditor(text) => {
                    let content = suspended_editor(guard, suspended, &text);
                    queue.extend(update(&mut state, Msg::EditorDone(content)));
                }
                // The client-side half of `/reload`. The engine half is a wire
                // request `exec_wire_cmd` already sent; these are the settings
                // no server knows about, so they are re-read here and applied
                // at once — the theme flips while the rebuild is still in
                // flight.
                Cmd::ReloadSettings => {
                    let s = client_settings();
                    palette = s.palette;
                    guard.set_mouse(s.mouse);
                    copy_on_select = s.copy_on_select;
                    queue.extend(update(
                        &mut state,
                        Msg::SettingsReloaded {
                            vim_mode: s.vim_mode,
                            density: s.density,
                            warnings: s.warnings,
                        },
                    ));
                }
                Cmd::Quit => return Ok(0),
                // `exec_wire_cmd` handled every other variant.
                handled => debug_assert!(false, "unhandled cmd: {handled:?}"),
            }
        }
    }
}

/// What actually leaves for the model: `@[path]` references expanded to file
/// contents, dropped image paths read and base64-encoded. Both happen here,
/// not in `hotl-tui`, because they touch the filesystem and the core crate
/// must stay pure. The consequence is the right one: the transcript shows
/// what the human typed, the model receives the expansion — matching how
/// `-p` already behaves.
///
/// A failed or over-cap read degrades exactly like `expand_file_refs`: the
/// `[Image #N]` token is annotated in place in the text and the entry
/// dropped — never a hard error. The caps are the server's own
/// (`crate::images`), so a well-behaved client never sends what the server
/// would reject.
fn outbound(p: hotl_tui::paste::PromptPayload) -> hotl_tui::paste::PromptPayload {
    let mut text = crate::setup::expand_file_refs(&p.text);
    let mut images = Vec::new();
    for img in p.images {
        if images.len() >= crate::images::MAX_IMAGES_PER_PROMPT {
            let note = format!(
                "{} (image {} not attached: at most {} images per prompt)",
                img.marker,
                img.path,
                crate::images::MAX_IMAGES_PER_PROMPT
            );
            text = text.replace(&img.marker, &note);
            continue;
        }
        match load_image(&img.path, crate::images::MAX_IMAGE_DECODED_BYTES as u64) {
            Ok(data) => images.push(hotl_tui::paste::ImageAttachment {
                data: Some(data),
                ..img
            }),
            Err(why) => {
                let note = format!(
                    "{} (image {} could not be attached: {why})",
                    img.marker, img.path
                );
                text = text.replace(&img.marker, &note);
            }
        }
    }
    hotl_tui::paste::PromptPayload { text, images }
}

/// Read + base64-encode one dropped image. Metadata first, so an over-cap
/// file is refused without reading it. `~` expands to `$HOME` — terminals
/// hand us the path exactly as dropped. Blocking on the loop thread is
/// accepted precedent at this seam: `history.append` already does blocking
/// fs here, and `$EDITOR` suspends the loop outright.
fn load_image(path: &str, cap: u64) -> Result<String, String> {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => std::path::PathBuf::from(home).join(rest),
            None => std::path::PathBuf::from(path),
        },
        None => std::path::PathBuf::from(path),
    };
    let meta = std::fs::metadata(&expanded).map_err(|e| e.to_string())?;
    if meta.len() > cap {
        return Err(format!(
            "{:.1}MB exceeds the {}MB per-image cap",
            meta.len() as f64 / (1024.0 * 1024.0),
            cap / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(&expanded).map_err(|e| e.to_string())?;
    Ok(hotl_tools::b64::encode(&bytes))
}

/// Put a screen region on the clipboard; returns the lines actually copied.
///
/// The region is resolved against a freshly rendered frame rather than the
/// loop's own: `Terminal::draw` swaps buffers on the way out, so the drawn
/// frame is reachable only through the `CompletedFrame` it returns, and that
/// borrow cannot be held across the loop's `select!`. Re-drawing costs one
/// frame per button release and yields exactly what the user is looking at,
/// highlight included.
///
/// Transport is OSC 52 — no clipboard crate, and it works over SSH. A
/// screen-bounded region is far under every terminal's payload cap, so it goes
/// in one write. Terminals that refuse OSC 52 (or tmux without
/// `set-clipboard on`) simply drop it; there is no reply to check.
/// The OSC 52 clipboard write: `ESC ] 52 ; c ; <base64> BEL`. Base64 is what
/// makes the payload safe to embed — newlines and control bytes in the copied
/// text cannot terminate the sequence early.
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", hotl_tools::b64::encode(text.as_bytes()))
}

fn copy_region(
    guard: &mut TerminalGuard,
    state: &State,
    palette: &Palette,
    sel: &hotl_tui::select::Selection,
) -> io::Result<usize> {
    use std::io::Write;
    let frame = guard.terminal.draw(|f| view(state, palette, f))?;
    let text = hotl_tui::view::selection_text(state, frame.buffer, sel);
    if text.is_empty() {
        return Ok(0);
    }
    let mut out = io::stdout();
    let _ = write!(out, "{}", osc52(&text));
    let _ = out.flush();
    Ok(text.lines().count())
}

/// Transcript items per wheel notch — a third of a page (`scroll::PAGE`).
const WHEEL_LINES: usize = 3;

/// One terminal event → an Elm message, or `None` for events the loop only
/// needs as a redraw trigger (resize, focus, mouse motion, key release). Pure,
/// so the select arm's mapping is unit-testable.
///
/// The `None` for the remaining mouse kinds is load-bearing rather than
/// defensive: it drops motion and click noise before any `Msg` exists, so the
/// mouse cannot drive a redraw storm through the loop. Left drags are the one
/// exception, and only when `select` is on. They are safe to admit because
/// button-less motion (mode 1003) is still dropped here, and mode 1002 reports
/// at most one `Drag` per *cell crossed* — so the redraw rate is bounded by
/// the width of the terminal, not by how fast the pointer is sampled.
fn terminal_msg(ev: Event, select: bool) -> Option<Msg> {
    use crossterm::event::{MouseButton, MouseEventKind};
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => Some(Msg::Key(k)),
        // Bracketed paste: the terminal hands us the whole payload as one
        // event instead of one `Enter` per line.
        Event::Paste(text) => Some(Msg::Paste(text)),
        Event::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => {
                Some(Msg::Scroll(hotl_tui::scroll::Intent::Up(WHEEL_LINES)))
            }
            MouseEventKind::ScrollDown => {
                Some(Msg::Scroll(hotl_tui::scroll::Intent::Down(WHEEL_LINES)))
            }
            MouseEventKind::Down(MouseButton::Left) if select => Some(Msg::SelectStart {
                col: m.column,
                row: m.row,
            }),
            MouseEventKind::Drag(MouseButton::Left) if select => Some(Msg::SelectExtend {
                col: m.column,
                row: m.row,
            }),
            MouseEventKind::Up(MouseButton::Left) if select => Some(Msg::SelectEnd),
            _ => None,
        },
        _ => None,
    }
}

/// Mouse capture: `HOTL_MOUSE` beats `[behavior] mouse`, per the
/// env > config.toml > default precedence in `config.rs`. Split out from the
/// startup path so it is testable without touching the process environment.
fn resolve_mouse(env: Option<&str>, cfg: bool) -> bool {
    if env == Some("0") {
        false
    } else {
        cfg
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

#[cfg(test)]
mod tests {
    use super::{
        osc52, parse_tui_args, resolve_mouse, resolve_session_arg, terminal_msg, WHEEL_LINES,
    };
    use crossterm::event::{Event, KeyModifiers};
    use hotl_tui::app::Msg;
    use serde_json::json;
    use std::time::SystemTime;

    use hotl_tui::paste::{ImageAttachment, PromptPayload};

    fn payload(text: &str, images: Vec<ImageAttachment>) -> PromptPayload {
        PromptPayload {
            text: text.into(),
            images,
        }
    }

    fn attachment(marker: &str, path: &str) -> ImageAttachment {
        ImageAttachment {
            marker: marker.into(),
            path: path.into(),
            media_type: "image/png".into(),
            data: None,
        }
    }

    #[test]
    fn file_refs_expand_on_the_way_out_not_in_the_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.txt");
        std::fs::write(&p, "CONTENTS").unwrap();
        let typed = format!("look at @[{}]", p.display());
        assert!(super::outbound(payload(&typed, Vec::new()))
            .text
            .contains("CONTENTS"));
        assert_eq!(super::outbound(payload("plain", Vec::new())).text, "plain");
    }

    #[test]
    fn outbound_encodes_a_readable_image_and_annotates_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("shot.png");
        std::fs::write(&img, b"fake png bytes").unwrap();
        let good = attachment("[Image #1]", &img.display().to_string());
        let gone = attachment("[Image #2]", "/definitely/not/here.png");
        let out = super::outbound(payload("see [Image #1] and [Image #2]", vec![good, gone]));
        // The readable one is encoded in place…
        assert_eq!(out.images.len(), 1);
        assert_eq!(
            out.images[0].data.as_deref(),
            Some(hotl_tools::b64::encode(b"fake png bytes").as_str())
        );
        // …and the missing one degrades to an inline note, never an error.
        assert!(out.text.contains("[Image #1]"), "{}", out.text);
        assert!(
            out.text
                .contains("[Image #2] (image /definitely/not/here.png could not be attached"),
            "{}",
            out.text
        );
    }

    #[test]
    fn load_image_refuses_an_over_cap_file_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("big.png");
        std::fs::write(&img, vec![0u8; 64]).unwrap();
        let err = super::load_image(&img.display().to_string(), 16).unwrap_err();
        assert!(err.contains("per-image cap"), "{err}");
    }

    #[test]
    fn handshake_reads_the_mode_and_context_window() {
        let hello = json!({"defaultMode": "auto", "contextWindow": 1_000_000});
        let opened = json!({"sessionId": "s", "name": null, "mode": "plan"});
        let (mode, window) = super::open_settings(&hello, &opened);
        assert_eq!(mode, "plan", "the session's mode beats the server default");
        assert_eq!(window, 1_000_000);

        // An older server that reports neither must not brick the client.
        let (mode, window) = super::open_settings(&json!({}), &json!({}));
        assert_eq!(mode, "ask");
        assert_eq!(window, hotl_tui::app::DEFAULT_CONTEXT_WINDOW);
    }

    /// A mouse event at `(col, row)`; column and row only matter for drags.
    fn mouse_at(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn wheel_events_become_scroll_messages() {
        use crossterm::event::MouseEventKind;
        let ev = |kind| mouse_at(kind, 0, 0);
        assert_eq!(
            terminal_msg(ev(MouseEventKind::ScrollUp), true),
            Some(Msg::Scroll(hotl_tui::scroll::Intent::Up(WHEEL_LINES)))
        );
        assert_eq!(
            terminal_msg(ev(MouseEventKind::ScrollDown), true),
            Some(Msg::Scroll(hotl_tui::scroll::Intent::Down(WHEEL_LINES)))
        );
        // Button-less motion stays noise even with selection on — mode 1003
        // reports it constantly and it must not wake the update loop.
        assert_eq!(terminal_msg(ev(MouseEventKind::Moved), true), None);
        assert_eq!(terminal_msg(ev(MouseEventKind::Moved), false), None);
    }

    #[test]
    fn a_left_drag_becomes_selection_messages() {
        use crossterm::event::{MouseButton, MouseEventKind};
        assert_eq!(
            terminal_msg(
                mouse_at(MouseEventKind::Down(MouseButton::Left), 4, 2),
                true
            ),
            Some(Msg::SelectStart { col: 4, row: 2 })
        );
        assert_eq!(
            terminal_msg(
                mouse_at(MouseEventKind::Drag(MouseButton::Left), 9, 5),
                true
            ),
            Some(Msg::SelectExtend { col: 9, row: 5 })
        );
        assert_eq!(
            terminal_msg(mouse_at(MouseEventKind::Up(MouseButton::Left), 9, 5), true),
            Some(Msg::SelectEnd)
        );
    }

    #[test]
    fn a_left_drag_stays_dropped_when_copy_on_select_is_off() {
        use crossterm::event::{MouseButton, MouseEventKind};
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert_eq!(
                terminal_msg(mouse_at(kind, 4, 2), false),
                None,
                "{kind:?} must be dropped before any Msg exists"
            );
        }
    }

    #[test]
    fn other_mouse_buttons_are_never_selection() {
        use crossterm::event::{MouseButton, MouseEventKind};
        for button in [MouseButton::Right, MouseButton::Middle] {
            assert_eq!(
                terminal_msg(mouse_at(MouseEventKind::Down(button), 4, 2), true),
                None
            );
            assert_eq!(
                terminal_msg(mouse_at(MouseEventKind::Drag(button), 4, 2), true),
                None
            );
        }
    }

    #[test]
    fn the_clipboard_escape_is_a_well_formed_osc_52() {
        // `ESC ] 52 ; c ; <base64> BEL` — `c` is the clipboard selection.
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
        // Newlines are payload, not terminators: a multi-line copy must not
        // truncate at the first one.
        assert_eq!(osc52("a\nb"), "\x1b]52;c;YQpi\x07");
    }

    #[test]
    fn hotl_mouse_env_overrides_the_config_key() {
        // Precedence is env > config.toml > default (config.rs module doc).
        assert!(!resolve_mouse(Some("0"), true), "env off beats config on");
        assert!(resolve_mouse(None, true));
        assert!(!resolve_mouse(None, false), "config off is honored");
        assert!(resolve_mouse(Some("1"), true));
    }

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
