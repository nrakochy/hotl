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
use hotl_platform::KnownPaths as _;
use hotl_theme::Palette;
use hotl_tui::app::{update, Cmd, Msg, Phase, State};
use hotl_tui::client::{exec_wire_cmd, read_server_msg, translate, AcpClient, ServerMsg};
use hotl_tui::view::{view, TranscriptCache};

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
    let tui_args = match parse_tui_args(&args) {
        Ok(a) => a,
        Err(code) => return code,
    };
    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    // Prompt-history tail, loaded (and startup-compacted) before the screen is
    // taken; the store is handed to the loop to append each submitted prompt.
    // Startup-only on purpose — `/reload` does not re-read `[history]`.
    let (history_store, history) =
        crate::history::History::load(&cfg.history, &crate::agent::data_dir());
    // Deferred to a transcript notice: with the composer painted immediately
    // (0033 Task 8b), a stderr line would only flash under the alt screen.
    let first_run_hint = crate::setup::first_run_hint(&crate::agent::config_dir());
    // The same read `/reload` repeats. Warnings print as plain lines here,
    // before the alternate screen owns stdout; on reload they become notices.
    let settings = client_settings();
    for w in &settings.warnings {
        eprintln!("hotl: {w}");
    }

    let suspended = Arc::new(AtomicBool::new(false));
    let mut keys = spawn_key_reader(suspended.clone());
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

    // 0033 Task 8b: paint the composer NOW and hydrate behind it. Nothing
    // about session hydration is a render dependency — the one true
    // sequencing constraint (session-start items commit before the first
    // prompt commit) gates the first *send*, and sends only exist in the
    // post-open loop. The neutral state renders NO mode badge (never a
    // guessed one) and a quiet "starting" strip.
    let mut state = State::new(settings.vim_mode, String::new());
    state.mode = String::new();
    state.density = settings.density;
    state.editor.load_history(history);
    let mut cache = TranscriptCache::default();
    let mut ticker = tokio::time::interval(Duration::from_millis(1_000 / hotl_tui::anim::TICK_HZ));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut open = std::pin::pin!(open_session(tui_args));
    let opened = loop {
        if let Err(e) = guard
            .terminal
            .draw(|f| view(&state, &settings.palette, &mut cache, f))
        {
            drop(guard);
            eprintln!("hotl: {e}");
            return 1;
        }
        tokio::select! {
            o = &mut open => break o,
            ev = keys.recv() => match ev {
                Some(ev) => {
                    if let Some(msg) = terminal_msg(ev, settings.copy_on_select) {
                        hotl_tui::app::pre_open_input(&mut state, msg);
                    }
                }
                None => return 1,
            },
            _ = ticker.tick() => {}
        }
    };
    let (mut client, mut reader, model, opened) = match opened {
        Ok(o) => o,
        Err(OpenFail::Code(code)) => {
            drop(guard);
            // The factory's own diagnostics went to the (now discarded) alt
            // screen; leave a pointer that survives the restore.
            eprintln!(
                "hotl: session startup failed — run `hotl doctor` (or `hotl -p \"hi\"`) to see the error"
            );
            return code;
        }
        Err(OpenFail::Msg(e)) => {
            drop(guard);
            eprintln!("hotl: {e}");
            return 1;
        }
    };
    let Opened {
        name: session_name,
        skills,
        mode,
        plan,
        default_effort,
        goal,
        context_window,
    } = opened;
    state.model = model;
    state.session_name = session_name;
    // Server-side truth, seeded before the post-open loop's first draw: the
    // badge must never render a mode the session is not actually running
    // (evaluation §5.7) — pre-open it rendered none at all.
    state.mode = mode;
    state.plan = plan;
    state.default_effort = default_effort;
    // A resumed goal's counters start at zero (reset-on-resume, per docs).
    state.goal = goal;
    state.context_window = context_window;
    state.set_skills(skills);
    if let Some(hint) = first_run_hint {
        state
            .transcript
            .push(hotl_tui::app::TranscriptItem::Notice { text: hint.into() });
    }
    // A prompt typed and entered during the open window fires now, through
    // the normal submit path — exactly one SendPrompt, history included.
    let initial: VecDeque<Cmd> = hotl_tui::app::fire_queued_submit(&mut state).into();
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
        cache,
        initial,
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

/// How an [`open_session`] failure surfaces: the factory exits with a code
/// (it printed its own diagnostics), the handshake with a message.
enum OpenFail {
    Code(i32),
    Msg(String),
}

/// The whole open sequence — factory, in-process server, handshake — as one
/// future the pre-open loop drives concurrently with typing (0033 Task 8b).
async fn open_session(
    tui_args: TuiArgs,
) -> Result<(Client, ServerReader, String, Opened), OpenFail> {
    let (factory, model, info) = crate::agent::acp_factory().await.map_err(OpenFail::Code)?;
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server_io);
    // `serve` with the reload hook: `/reload` asks the engine to rebuild
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
    let opened = handshake(&mut client, &mut reader, tui_args)
        .await
        .map_err(OpenFail::Msg)?;
    Ok((client, reader, model, opened))
}

/// Session settings from the handshake pair. The session's own `mode` wins
/// over the server default (a resumed session inherits and coerces its own);
/// both fall back so a newer client against an older server still runs — it
/// simply shows the library default instead of guessing.
fn open_settings(hello: &Value, opened: &Value) -> (String, bool, u64) {
    let mode = opened
        .get("mode")
        .or_else(|| hello.get("defaultMode"))
        .and_then(Value::as_str)
        .unwrap_or("ask")
        .to_string();
    let plan = opened
        .get("plan")
        .or_else(|| hello.get("defaultPlan"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let window = hello
        .get("contextWindow")
        .and_then(Value::as_u64)
        .filter(|&w| w > 0)
        .unwrap_or(hotl_tui::app::DEFAULT_CONTEXT_WINDOW);
    (mode, plan, window)
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
    plan: bool,
    /// The session's resolved starting effort — display-only, what a bare
    /// `/effort` reports when the user has set nothing (0030 Task 8).
    default_effort: Option<String>,
    /// The active goal a resume restored (0034); a fork's or a fresh
    /// session's is `None`. Counters start at zero — reset-on-resume.
    goal: Option<String>,
    context_window: u64,
}

/// initialize + session/new|load before entering raw mode, so wiring errors
/// print as plain lines instead of corrupting an alt-screen.
async fn handshake(
    client: &mut Client,
    reader: &mut ServerReader,
    args: TuiArgs,
) -> Result<Opened, String> {
    let init = client.request("initialize", Value::Null).await;
    let hello = wait_response(reader, init).await?;
    let skills = hotl_tui::client::parse_skills(&hello);
    let TuiArgs {
        spec,
        name,
        fork_from,
        keep,
        keep_turns,
    } = args;
    // A fork rides `session/load` as extension params rather than a new
    // method: existing clients ignore what they don't know, so the wire stays
    // backward-compatible. Turn resolution is deliberately server-side — the
    // projection those turns are counted against only exists there.
    let open = match (spec, fork_from) {
        (_, Some(sid)) => {
            let mut params = json!({"sessionId": sid, "name": name, "fork": true});
            if let Some(n) = keep {
                params["keepItems"] = json!(n);
            }
            if let Some(t) = keep_turns {
                params["keepTurns"] = json!(t);
            }
            client.request("session/load", params).await
        }
        (None, None) => client.request("session/new", json!({"name": name})).await,
        (Some(sid), None) => {
            client
                .request("session/load", json!({"sessionId": sid, "name": name}))
                .await
        }
    };
    let v = wait_response(reader, open).await?;
    let (mode, plan, context_window) = open_settings(&hello, &v);
    Ok(Opened {
        name: v.get("name").and_then(Value::as_str).map(String::from),
        skills,
        mode,
        plan,
        default_effort: v
            .get("defaultEffort")
            .and_then(Value::as_str)
            .map(String::from),
        goal: v.get("goal").and_then(Value::as_str).map(String::from),
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

/// A streamed delta defers its draw to the armed tick; anything else —
/// keys, tool events, TurnDone, Tick itself — draws now. `child_tool` rides
/// the tick too (0039 D9): children arrive at up to 4× the parent's tool
/// rate, and the 30 Hz repaint caps the deferral at ≤ 33 ms.
fn defers_draw(msg: &Msg) -> bool {
    matches!(msg, Msg::Update(v)
        if matches!(v.get("type").and_then(Value::as_str),
            Some("text_delta" | "thinking_delta" | "child_tool")))
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
    // Carried over from the pre-open loop (0033 Task 8b) so the transition
    // does not re-wrap what is already rendered.
    mut cache: TranscriptCache,
    // Startup commands — the queued pre-open submit — drained before the
    // first select, through the same machinery every per-msg command uses.
    mut queue: VecDeque<Cmd>,
) -> io::Result<i32> {
    let mut prompt_ids: VecDeque<u64> = VecDeque::new();
    let mut steer_ids: VecDeque<u64> = VecDeque::new();
    // The animation's own rate, armed only while a turn runs — idle schedules
    // no wakeups, which is also why the idle wave is a frozen frame.
    let mut ticker = tokio::time::interval(Duration::from_millis(1_000 / hotl_tui::anim::TICK_HZ));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // 0033 Task 2: deltas mark the frame dirty and ride the 30 Hz tick that
    // is armed exactly when deltas can arrive; everything else draws now.
    let mut needs_draw = true;
    loop {
        while let Some(cmd) = queue.pop_front() {
            // The wire-bound half is shared with the e2e harness; what comes
            // back is the terminal-bound remainder this loop owns.
            // `@[file]` refs and image paths expand on the way out only.
            let cmd = match cmd {
                Cmd::SendPrompt(p) => Cmd::SendPrompt(outbound(p).await),
                Cmd::SendSteer(p) => Cmd::SendSteer(outbound(p).await),
                other => other,
            };
            let Some(cmd) = exec_wire_cmd(cmd, client, &mut prompt_ids, &mut steer_ids).await
            else {
                continue;
            };
            match cmd {
                Cmd::SetTitle(title) => {
                    let _ = execute!(io::stdout(), SetTitle(&title));
                }
                Cmd::AppendHistory(text) => history.append(&text),
                Cmd::CopySelection(sel) => {
                    let lines = copy_region(guard, &state, &palette, &mut cache, &sel)?;
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
        // No `needs_draw = false` here: every path below reassigns it before
        // this line runs again (redraw-only `continue`, or the loop bottom).
        if needs_draw {
            guard
                .terminal
                .draw(|f| view(&state, &palette, &mut cache, f))?;
        }
        let msg = tokio::select! {
            ev = keys.recv() => match ev {
                Some(ev) => terminal_msg(ev, copy_on_select), // `None` = redraw-only (resize, mouse motion)
                None => return Ok(1),
            },
            sm = read_server_msg(reader) => match sm {
                Some(m) => translate(m, &mut prompt_ids, &mut steer_ids),
                None => return Ok(1), // server hung up
            },
            _ = ticker.tick(), if state.phase != Phase::Idle => Some(Msg::Tick),
        };
        let Some(msg) = msg else {
            needs_draw = true; // redraw-only: resize, mouse motion
            continue;
        };
        // Decided before `update` consumes the msg; applied after the cmd
        // queue drains at the top of the next iteration. The Idle guard
        // covers the impossible-but-cheap case of a delta arriving with no
        // tick armed to flush it.
        let defer = defers_draw(&msg);
        queue = update(&mut state, msg).into();
        needs_draw = !defer || state.phase == Phase::Idle;
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
/// (`crate::images`) — count (`MAX_IMAGES_PER_PROMPT`), per-image
/// (`MAX_IMAGE_DECODED_BYTES`), and per-prompt total
/// (`MAX_PROMPT_DECODED_BYTES`) — so a well-behaved client never sends what
/// the server would reject.
async fn outbound(p: hotl_tui::paste::PromptPayload) -> hotl_tui::paste::PromptPayload {
    let mut text = crate::setup::expand_file_refs(&p.text);
    let mut images = Vec::new();
    let mut total = 0usize;
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
        // Cannot underflow: `total` grows only by `decoded`, which is bounded
        // by `cap = min(MAX_IMAGE_DECODED_BYTES, remaining) <= remaining`, so
        // `total` never exceeds `MAX_PROMPT_DECODED_BYTES`.
        let remaining = crate::images::MAX_PROMPT_DECODED_BYTES - total;
        let cap = (crate::images::MAX_IMAGE_DECODED_BYTES).min(remaining);
        let path = img.path.clone();
        let loaded = tokio::task::spawn_blocking(move || load_image(&path, cap as u64))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        match loaded {
            Ok((data, decoded)) => {
                total += decoded;
                images.push(hotl_tui::paste::ImageAttachment {
                    data: Some(data),
                    ..img
                });
            }
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
/// hand us the path exactly as dropped. Stays synchronous and runs on
/// `spawn_blocking`: `history.append` writes a few hundred bytes and can
/// stay on the loop thread, but eight 5MB reads plus base64, run inline on
/// `block_on`'s `current_thread` runtime, would stall every other task
/// sharing its one worker — chiefly the in-process `acp::serve` task, which
/// `spawn_blocking` genuinely frees to keep running. It does not free the
/// wire reader: that lives in this same run-loop task as the caller and
/// stays unpolled for the `.await`'s duration either way.
fn load_image(path: &str, cap: u64) -> Result<(String, usize), String> {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => match hotl_platform::KNOWN_PATHS.home() {
            Some(home) => home.join(rest),
            None => std::path::PathBuf::from(path),
        },
        None => std::path::PathBuf::from(path),
    };
    let meta = std::fs::metadata(&expanded).map_err(|e| e.to_string())?;
    if meta.len() > cap {
        return Err(if cap < crate::images::MAX_IMAGE_DECODED_BYTES as u64 {
            if cap == 0 {
                "no room left in the per-prompt image budget".into()
            } else {
                format!(
                    "only {:.1}MB of the per-prompt image budget is left",
                    cap as f64 / (1024.0 * 1024.0)
                )
            }
        } else {
            format!(
                "{:.1}MB exceeds the {}MB per-image cap",
                meta.len() as f64 / (1024.0 * 1024.0),
                cap / (1024 * 1024)
            )
        });
    }
    if meta.len() == 0 {
        // The server's `decoded_len` rejects "" and takes the whole prompt
        // with it; refusing here degrades one attachment instead.
        return Err("is empty".into());
    }
    let encoded = {
        let bytes = std::fs::read(&expanded).map_err(|e| e.to_string())?;
        hotl_tools::b64::encode(&bytes)
    };
    Ok((encoded, meta.len() as usize))
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
/// Delivery is `clipboard::copy`, which fans out to every transport that
/// applies — the OSC 52 escape for SSH and OSC-52-capable terminals, a tmux
/// route that survives the `set-clipboard`/`allow-passthrough` defaults that
/// drop a bare escape, and a native program for terminals that never spoke
/// OSC 52. A screen-bounded region is far under every sink's payload cap.
fn copy_region(
    guard: &mut TerminalGuard,
    state: &State,
    palette: &Palette,
    cache: &mut TranscriptCache,
    sel: &hotl_tui::select::Selection,
) -> io::Result<usize> {
    let frame = guard.terminal.draw(|f| view(state, palette, cache, f))?;
    let text = hotl_tui::view::selection_text(state, frame.buffer, sel);
    if text.is_empty() {
        return Ok(0);
    }
    crate::clipboard::copy(&mut io::stdout(), &text);
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
) -> Result<Option<String>, String> {
    suspended.store(true, Ordering::Relaxed);
    guard.suspend();
    let content = run_external_editor(text);
    guard.resume();
    suspended.store(false, Ordering::Relaxed);
    content
}

/// Blocking is fine — the TUI is suspended.
///
/// `Ok(None)` is "nothing changed": the file came back identical, or the editor
/// was aborted. `Err` is "the editor never ran", and the two used to be
/// indistinguishable — both collapsed to `None`, so on a box with no POSIX
/// shell the key did nothing at all, silently, forever. A user cannot tell a
/// no-op from a missing shell, so the failure has to say so.
fn run_external_editor(text: &str) -> Result<Option<String>, String> {
    // The resolved shell, not the literal name `sh`.
    let shell = hotl_tools::shell::resolve_shell()
        .map_err(|why| format!("cannot open $EDITOR: no POSIX shell resolved — {why}"))?;
    let path = std::env::temp_dir().join(format!("hotl-tui-{}.md", std::process::id()));
    std::fs::write(&path, text)
        .map_err(|e| format!("cannot stage the draft at {}: {e}", path.display()))?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&shell.path)
        .arg("-c")
        .arg(format!("{editor} '{}'", path.display()))
        .status();
    let content = match status {
        Ok(s) if s.success() => Ok(std::fs::read_to_string(&path).ok()),
        // A non-zero exit is how `vi`'s `:cq` says "discard this" — an abort,
        // not a failure. Only a spawn failure means the editor never ran.
        Ok(_) => Ok(None),
        Err(e) => Err(format!("could not start `{editor}`: {e}")),
    };
    let _ = std::fs::remove_file(&path);
    content.map(|c| c.filter(|c| c.trim_end() != text.trim_end()))
}

#[derive(Debug)]
pub(crate) struct TuiArgs {
    pub spec: Option<String>,
    pub name: Option<String>,
    /// A resolved session id to **fork** from, mutually exclusive with `spec`.
    pub fork_from: Option<String>,
    pub keep: Option<usize>,
    pub keep_turns: Option<usize>,
}

/// The console's flags as typed, before any of them touch the store. Split out
/// so the rules about which combinations mean anything are a pure function
/// with real messages, rather than something only reachable through a live
/// sessions directory.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RawTuiArgs {
    pub spec: Option<String>,
    /// True when `spec` came from a bare positional rather than `-r`. The two
    /// resolve differently and always have: a positional is an id-prefix only,
    /// while `-r` also accepts a picker list number and a session name.
    pub spec_is_positional: bool,
    pub resume_bare: bool,
    pub name: Option<String>,
    pub fork_from: Option<String>,
    pub keep: Option<usize>,
    pub keep_turns: Option<usize>,
}

/// The console's argument surface: `[id-prefix]`, `-r/--resume [arg]`,
/// `-n/--name <name>`, `--fork-from <arg>` with `--keep`/`--keep-turns`.
/// `hotl resume [arg]` arrives here rewritten to `--resume` by main.rs.
fn parse_tui_flags(args: &[String]) -> Result<RawTuiArgs, String> {
    let mut out = RawTuiArgs::default();
    let mut it = args.iter().peekable();
    let value = |it: &mut std::iter::Peekable<std::slice::Iter<String>>, flag: &str| {
        it.next()
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            // The one-time migration hint: `tui` was a subcommand before the
            // default flip, and would otherwise read as a session-id prefix.
            "tui" => {
                return Err(
                    "the TUI is now just `hotl` (the `tui` subcommand was removed)".to_string(),
                )
            }
            "-r" | "--resume" => match it.peek() {
                Some(v) if !v.starts_with('-') => out.spec = it.next().cloned(),
                _ => out.resume_bare = true,
            },
            "--fork-from" => out.fork_from = Some(value(&mut it, "--fork-from")?),
            "--keep" => out.keep = Some(count(&value(&mut it, "--keep")?, "--keep")?),
            "--keep-turns" => {
                out.keep_turns = Some(count(&value(&mut it, "--keep-turns")?, "--keep-turns")?)
            }
            "-n" | "--name" => {
                match it
                    .next()
                    .map(String::as_str)
                    .and_then(hotl_types::normalize_session_name)
                {
                    Some(n) => out.name = Some(n),
                    None => return Err("-n/--name needs a value of 1–64 chars".to_string()),
                }
            }
            // Start with the plan overlay on; `/plan` toggles it after. Rides
            // the env var `load_rules` reads, same as the headless `--plan`.
            "--plan" => std::env::set_var("HOTL_PLAN", "1"),
            flag if flag.starts_with('-') => {
                return Err(format!("unknown argument `{flag}` (try --help)"))
            }
            prefix => {
                out.spec = Some(prefix.to_string());
                out.spec_is_positional = true;
            }
        }
    }
    check_lineage_flags(
        out.spec.is_some() || out.resume_bare,
        out.fork_from.is_some(),
        out.keep,
        out.keep_turns,
    )?;
    Ok(out)
}

fn count(raw: &str, flag: &str) -> Result<usize, String> {
    raw.parse()
        .map_err(|_| format!("{flag} needs a non-negative whole number, got `{raw}`"))
}

/// Resume and fork are two different things to do with one session, and the
/// keep flags are two coordinate systems onto one cut. Shared with the
/// headless parser so `hotl -p` and the console reject the same nonsense in
/// the same words.
pub(crate) fn check_lineage_flags(
    resuming: bool,
    forking: bool,
    keep: Option<usize>,
    keep_turns: Option<usize>,
) -> Result<(), String> {
    if resuming && forking {
        return Err(
            "either resume or fork a session, not both — a fork always starts a new \
                    session, so -r has nothing left to mean"
                .to_string(),
        );
    }
    if keep.is_some() && keep_turns.is_some() {
        return Err("pass one of --keep or --keep-turns, not both".to_string());
    }
    if !forking && (keep.is_some() || keep_turns.is_some()) {
        return Err(
            "--keep/--keep-turns requires --fork-from — there is nothing to cut back \
                    without a session to fork"
                .to_string(),
        );
    }
    Ok(())
}

fn parse_tui_args(args: &[String]) -> Result<TuiArgs, i32> {
    let raw = parse_tui_flags(args).map_err(|e| {
        eprintln!("hotl: {e}");
        2
    })?;
    let RawTuiArgs {
        spec,
        spec_is_positional,
        resume_bare,
        name,
        fork_from,
        keep,
        keep_turns,
    } = raw;
    let sessions = newest_first();
    let spec = match spec {
        Some(arg) if spec_is_positional => Some(by_prefix(&arg)?),
        Some(arg) => Some(resolve_session_arg(&arg, &sessions)?),
        None if resume_bare => Some(pick_session()?),
        None => None,
    };
    let fork_from = match fork_from {
        Some(arg) => {
            let id = resolve_session_arg(&arg, &sessions)?;
            // Library code never prints; this is the CLI, and the honest
            // framing of the feature's economics belongs where the user can
            // still change their mind about running the phases back-to-back.
            if let Some(note) = crate::agent::cold_cache_note(
                &crate::agent::sessions_dir(),
                &id,
                hotl_provider::CacheTtl::OneHour,
            ) {
                eprintln!("hotl: {note}");
            }
            Some(id)
        }
        None => None,
    };
    Ok(TuiArgs {
        spec,
        name,
        fork_from,
        keep,
        keep_turns,
    })
}

/// `-r <arg>` resolution: picker list number → unique id-prefix → unique
/// exact name. Ambiguity errors; it never falls through past a hit.
fn resolve_session_arg(
    arg: &str,
    sessions: &[(String, PathBuf, SystemTime)],
) -> Result<String, i32> {
    // The picker list number is the console's own coordinate — it means
    // nothing headless, so it lives here rather than in the shared resolver.
    if let Ok(n) = arg.parse::<usize>() {
        if (1..=sessions.len().min(20)).contains(&n) {
            return Ok(sessions[n - 1].0.clone());
        }
    }
    crate::agent::resolve_session_ref(arg, sessions).map_err(|e| {
        eprintln!("hotl: {e}");
        2
    })
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
    crate::agent::sessions_newest_first(&crate::agent::sessions_dir())
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
        // A header-only first-line read per row — cheap enough for a listing
        // that already stats every file.
        let from = hotl_store::session_parent(path)
            .map(|p| format!("  ↳ from {}", &p[..p.len().min(8)]))
            .unwrap_or_default();
        match hotl_store::session_name(path) {
            Some(name) => eprintln!("  {}) {id}  {name}  {}{from}", i + 1, age(*t)),
            None => eprintln!("  {}) {id}  {}{from}", i + 1, age(*t)),
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
        defers_draw, parse_tui_args, parse_tui_flags, resolve_mouse, resolve_session_arg,
        terminal_msg, WHEEL_LINES,
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

    /// 0033 Task 2: only streamed deltas ride the tick; every other message
    /// draws immediately.
    #[test]
    fn deltas_defer_everything_else_draws() {
        assert!(defers_draw(&Msg::Update(
            json!({"type": "text_delta", "text": "x"})
        )));
        assert!(defers_draw(&Msg::Update(
            json!({"type": "thinking_delta", "text": "x"})
        )));
        // 0039 D9: children arrive at up to 4× the parent's tool rate.
        assert!(defers_draw(&Msg::Update(
            json!({"type": "child_tool", "parent_id": "p", "id": "c", "name": "read", "summary": "read ./x", "phase": "start"})
        )));
        assert!(!defers_draw(&Msg::Update(
            json!({"type": "tool_start", "name": "bash", "summary": ""})
        )));
        assert!(!defers_draw(&Msg::Tick));
    }

    #[tokio::test]
    async fn file_refs_expand_on_the_way_out_not_in_the_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.txt");
        std::fs::write(&p, "CONTENTS").unwrap();
        let typed = format!("look at @[{}]", p.display());
        assert!(super::outbound(payload(&typed, Vec::new()))
            .await
            .text
            .contains("CONTENTS"));
        assert_eq!(
            super::outbound(payload("plain", Vec::new())).await.text,
            "plain"
        );
    }

    #[tokio::test]
    async fn outbound_encodes_a_readable_image_and_annotates_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("shot.png");
        std::fs::write(&img, b"fake png bytes").unwrap();
        let good = attachment("[Image #1]", &img.display().to_string());
        let gone = attachment("[Image #2]", "/definitely/not/here.png");
        let out = super::outbound(payload("see [Image #1] and [Image #2]", vec![good, gone])).await;
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
    fn load_image_refuses_an_over_budget_file_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("big.png");
        std::fs::write(&img, vec![0u8; 64]).unwrap();
        let err = super::load_image(&img.display().to_string(), 16).unwrap_err();
        assert!(err.contains("per-prompt image budget"), "{err}");
    }

    #[tokio::test]
    async fn a_zero_byte_file_is_refused_before_it_reaches_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("empty.png");
        std::fs::write(&img, b"").unwrap();
        let att = attachment("[Image #1]", &img.display().to_string());
        let out = super::outbound(payload("see [Image #1]", vec![att])).await;
        // The server's `decoded_len` rejects "" and takes the whole prompt with
        // it; refusing here degrades one attachment instead.
        assert!(out.images.is_empty());
        assert!(out.text.contains("[Image #1] (image"), "{}", out.text);
        assert!(out.text.contains("is empty"), "{}", out.text);
    }

    #[test]
    fn handshake_reads_the_mode_and_context_window() {
        let hello = json!({"defaultMode": "auto", "contextWindow": 1_000_000});
        let opened = json!({"sessionId": "s", "name": null, "mode": "plan"});
        let (mode, _plan, window) = super::open_settings(&hello, &opened);
        assert_eq!(mode, "plan", "the session's mode beats the server default");
        assert_eq!(window, 1_000_000);

        // An older server that reports neither must not brick the client.
        let (mode, _plan, window) = super::open_settings(&json!({}), &json!({}));
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
    fn fork_from_and_keep_are_parsed_and_are_incompatible_with_resume() {
        let a = parse_tui_flags(&v(&[
            "--fork-from",
            "sunny-day",
            "--keep",
            "12",
            "-n",
            "plan-b",
        ]))
        .unwrap();
        assert_eq!(a.fork_from.as_deref(), Some("sunny-day"));
        assert_eq!(a.keep, Some(12));
        assert_eq!(a.name.as_deref(), Some("plan-b"));

        let a = parse_tui_flags(&v(&["--fork-from", "@last", "--keep-turns", "3"])).unwrap();
        assert_eq!(a.fork_from.as_deref(), Some("@last"));
        assert_eq!(a.keep_turns, Some(3));

        let err = parse_tui_flags(&v(&["-r", "x", "--fork-from", "y"])).unwrap_err();
        assert!(
            err.contains("either resume or fork"),
            "one lineage verb per invocation: {err}"
        );
        let err = parse_tui_flags(&v(&["--keep", "3"])).unwrap_err();
        assert!(err.contains("requires --fork-from"), "{err}");
        let err = parse_tui_flags(&v(&[
            "--fork-from",
            "x",
            "--keep",
            "3",
            "--keep-turns",
            "1",
        ]))
        .unwrap_err();
        assert!(err.contains("one of --keep or --keep-turns"), "{err}");
        let err = parse_tui_flags(&v(&["--fork-from", "x", "--keep", "half"])).unwrap_err();
        assert!(err.contains("whole number"), "{err}");
    }

    #[test]
    fn at_last_resolves_to_the_newest_session() {
        let s = sessions(&["01AAA", "01BBB"]);
        assert_eq!(resolve_session_arg("@last", &s), Ok("01AAA".to_string()));
        assert_eq!(resolve_session_arg("@last", &[]).unwrap_err(), 2);
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
