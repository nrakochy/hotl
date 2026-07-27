//! End-to-end golden: the REAL stack minus the terminal — `acp::serve` with a
//! scripted provider ↔ in-process duplex ↔ the TUI's ACP client codec ↔ the
//! pure Elm core, rendered into a `TestBackend` after each step.

use std::collections::VecDeque;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hotl_engine::{spawn_session, EngineConfig, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_theme::Palette;
use hotl_tools::{rules::Rules, Registry};
use hotl_tui::app::{update, Cmd, Msg, Phase, State};
use hotl_tui::client::{exec_wire_cmd, read_server_msg, translate, AcpClient, ServerMsg};
use hotl_tui::view::view;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::{json, Value};
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};

// The server module lives in the binary crate; pull it in directly.
#[path = "../src/images.rs"]
#[allow(dead_code)]
mod images;

#[path = "../src/acp.rs"]
#[allow(dead_code)]
mod acp;

// `acp.rs` renders its frames through the shared renderer; pull that in too,
// so `crate::wire` resolves the same way it does in the binary.
#[path = "../src/wire.rs"]
#[allow(dead_code)]
mod wire;

type Reader = BufReader<ReadHalf<DuplexStream>>;
type Client = AcpClient<WriteHalf<DuplexStream>>;

/// A session whose scripted model calls bash (a gated tool → a permission
/// ask) then replies with text.
fn scripted_factory() -> acp::SessionFactory {
    Box::new(|_spec| {
        let dir = tempfile::tempdir().expect("tmp");
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).expect("log");
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
            ScriptedProvider::text_reply("all done via tui"),
        ]));
        // Keep the tempdir alive for the session's lifetime.
        std::mem::forget(dir);
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                provider,
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                snapshots: None,
                hooks: None,
                initial_items: Vec::new(),
                initial_todos: Vec::new(),
                config: EngineConfig {
                    max_turns: 6,
                    ..Default::default()
                },
            }),
            name: None,
            mode: "ask".into(),
        })
    })
}

/// A session whose scripted model asks a structured question (`ask_user`)
/// then replies with text — the tier-1 gap #4 golden.
fn scripted_ask_user_factory() -> acp::SessionFactory {
    Box::new(|_spec| {
        let dir = tempfile::tempdir().expect("tmp");
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).expect("log");
        let (cmd_tx, cmd_rx) = hotl_engine::session_channel();
        let (event_tx, event_rx) = hotl_engine::event_channel();
        let mut registry = Registry::builtin();
        let notifications = hotl_engine::hooks::NotificationDrain::new();
        registry.register(Box::new(hotl_tools::AskUserTool::new(
            hotl_engine::question_sink(
                cmd_tx.downgrade(),
                event_tx.downgrade(),
                None,
                notifications.clone(),
            ),
        )));
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call(
                "t1",
                "ask_user",
                json!({
                    "header": "Scope", "prompt": "How far?",
                    "options": [{"label": "MVP"}, {"label": "Full"}]
                }),
            ),
            ScriptedProvider::text_reply("all done via tui"),
        ]));
        std::mem::forget(dir);
        Ok(acp::SessionOpen {
            handle: hotl_engine::spawn_session_with_channels(
                SessionDeps {
                    provider,
                    registry: Arc::new(registry),
                    rules: Arc::new(Rules::default()),
                    sandbox_enforced: false,
                    clock: Arc::new(SystemClock),
                    log,
                    system: "sys".into(),
                    cwd: std::env::temp_dir(),
                    snapshots: None,
                    hooks: None,
                    initial_items: Vec::new(),
                    initial_todos: Vec::new(),
                    config: EngineConfig {
                        max_turns: 6,
                        ..Default::default()
                    },
                },
                cmd_tx,
                cmd_rx,
                event_tx,
                event_rx,
                notifications,
            ),
            name: None,
            mode: "ask".into(),
        })
    })
}

/// Spin up serve + client and complete the pre-TUI handshake.
async fn start() -> (Client, Reader) {
    start_with(scripted_factory()).await
}

async fn start_with(factory: acp::SessionFactory) -> (Client, Reader) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server_io);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        factory,
        acp::ServerInfo {
            skills: Vec::new(),
            default_mode: "ask".into(),
            context_window: 200_000,
            model: "m".into(),
        },
    ));
    let (cread, cwrite) = tokio::io::split(client_io);
    let mut client = AcpClient::new(cwrite);
    let mut reader = BufReader::new(cread);
    let init = client.request("initialize", Value::Null).await;
    wait_response(&mut reader, init).await.expect("initialize");
    let open = client.request("session/new", Value::Null).await;
    wait_response(&mut reader, open).await.expect("session/new");
    (client, reader)
}

async fn wait_response(reader: &mut Reader, want: u64) -> Result<Value, String> {
    loop {
        match next(reader).await {
            ServerMsg::Response { id, result } if id == want => return result,
            _ => {}
        }
    }
}

async fn next(reader: &mut Reader) -> ServerMsg {
    tokio::time::timeout(std::time::Duration::from_secs(5), read_server_msg(reader))
        .await
        .expect("server msg timeout")
        .expect("server hung up")
}

/// The runtime's own dispatch, not a copy of it: `translate` and
/// `exec_wire_cmd` are the same functions `hotl`'s run loop calls. The copies
/// this file used to carry had already drifted (§7) — the `translate` mirror
/// read only `/outcome/text`, so it could not have caught a bug in the real
/// one.
/// INVARIANT: this harness drives the real dispatch. Enforced by
/// `the_e2e_harness_uses_the_real_dispatch`.
async fn exec(cmds: Vec<Cmd>, client: &mut Client, prompt_ids: &mut VecDeque<u64>) {
    for cmd in cmds {
        // The terminal-bound remainder (title, editor, history, quit) has no
        // meaning in a headless test; the runtime handles those.
        let _ = exec_wire_cmd(cmd, client, prompt_ids).await;
    }
}

#[test]
fn the_e2e_harness_uses_the_real_dispatch() {
    let src = include_str!("tui_e2e.rs");
    assert!(
        !src.contains(concat!("Mirror of the ", "runtime")),
        "copies drift — call it"
    );
}

fn draw(state: &State) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| view(state, &Palette::default(), f))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect()
        })
        .collect()
}

const STRIP: usize = 19;

fn press(state: &mut State, code: KeyCode) -> Vec<Cmd> {
    update(state, Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

async fn type_prompt(
    state: &mut State,
    client: &mut Client,
    prompt_ids: &mut VecDeque<u64>,
    text: &str,
) {
    for c in text.chars() {
        press(state, KeyCode::Char(c));
    }
    let cmds = press(state, KeyCode::Enter);
    exec(cmds, client, prompt_ids).await;
}

#[tokio::test]
async fn prompt_stream_ask_allow_done_golden() {
    let (mut client, mut reader) = start().await;
    let mut state = State::new(true, "m".into());
    let mut prompt_ids = VecDeque::new();

    type_prompt(&mut state, &mut client, &mut prompt_ids, "go").await;
    assert!(
        draw(&state).iter().any(|r| r.contains("❯ go")),
        "prompt echoes immediately"
    );
    assert!(matches!(state.phase, Phase::Sampling { .. }));

    let mut saw_streaming_strip = false;
    loop {
        let Some(msg) = translate(next(&mut reader).await, &mut prompt_ids) else {
            continue;
        };
        let is_ask = matches!(msg, Msg::PermissionRequest { .. });
        let is_result = matches!(msg, Msg::PromptResult { .. });
        let cmds = update(&mut state, msg);
        exec(cmds, &mut client, &mut prompt_ids).await;
        if is_ask {
            let rows = draw(&state);
            assert!(
                rows.iter().any(|r| r.contains("bash")),
                "modal names the tool"
            );
            assert!(
                rows[STRIP].contains("╭─╮╰ ╯ waiting on you"),
                "halted gap glyph: {}",
                rows[STRIP]
            );
            // Allow it — the real server maps this to AskReply::Allow and the
            // turn continues: tool_done then turn_done arrive below.
            let cmds = press(&mut state, KeyCode::Char('y'));
            assert!(matches!(
                cmds[..],
                [Cmd::ReplyPermission { allow: true, .. }, ..]
            ));
            exec(cmds, &mut client, &mut prompt_ids).await;
        }
        if matches!(state.phase, Phase::Streaming { chars, .. } if chars > 0)
            && !saw_streaming_strip
        {
            saw_streaming_strip = true;
            let rows = draw(&state);
            assert!(
                rows[STRIP].contains("writing · ~"),
                "streaming strip approximates tokens: {}",
                rows[STRIP]
            );
        }
        if is_result {
            break;
        }
    }

    assert_eq!(state.phase, Phase::Idle);
    assert!(
        saw_streaming_strip,
        "text deltas streamed before the result"
    );
    let rows = draw(&state);
    assert!(
        rows.iter().any(|r| r.contains("✓ bash")),
        "tool card resolved"
    );
    assert!(
        rows.iter().any(|r| r.contains("all done via tui")),
        "assistant text rendered"
    );
    assert!(state.usage_line.is_some(), "real usage on the result");
    assert!(
        rows[STRIP].contains("· ─ ·"),
        "back to resting: {}",
        rows[STRIP]
    );
}

/// A dropped image path compacts to `[Image #1]` in the composer and echoes
/// as the same token in the transcript after submit — the display/wire fork
/// through the REAL dispatch (the harness runs without the runtime seam, so
/// the wire frame is text-only, exactly as it already skips `@[file]`
/// expansion; the JSON image shape is pinned in client.rs unit tests).
#[tokio::test]
async fn dropped_image_paste_compacts_and_echoes_golden() {
    let (mut client, _reader) = start().await;
    let mut state = State::new(true, "m".into());
    let mut prompt_ids = VecDeque::new();

    update(&mut state, Msg::Paste("/tmp/shot.png ".into()));
    for c in " what is this?".chars() {
        press(&mut state, KeyCode::Char(c));
    }
    let rows = draw(&state);
    assert!(
        rows.iter().any(|r| r.contains("[Image #1] what is this?")),
        "composer shows the token"
    );

    let cmds = press(&mut state, KeyCode::Enter);
    exec(cmds, &mut client, &mut prompt_ids).await;
    let rows = draw(&state);
    assert!(
        rows.iter().any(|r| r.contains("❯ [Image #1] what is this?")),
        "transcript echoes the token, not the path"
    );
    assert!(matches!(state.phase, Phase::Sampling { .. }));
}

/// `ask_user` end to end through the real TUI stack: the modal shows the
/// numbered options, a digit picks one, and the model's next turn (fed the
/// selected label as the tool result) completes normally. Also proves the
/// SECURITY invariant at the TUI layer: a question never freezes into
/// `session/request_permission` — only `session/request_question`.
#[tokio::test]
async fn ask_user_option_pick_golden() {
    let (mut client, mut reader) = start_with(scripted_ask_user_factory()).await;
    let mut state = State::new(true, "m".into());
    let mut prompt_ids = VecDeque::new();

    type_prompt(&mut state, &mut client, &mut prompt_ids, "go").await;

    loop {
        let Some(msg) = translate(next(&mut reader).await, &mut prompt_ids) else {
            continue;
        };
        assert!(
            !matches!(msg, Msg::PermissionRequest { .. }),
            "ask_user must never freeze into a permission ask"
        );
        let is_question = matches!(msg, Msg::QuestionRequest { .. });
        let is_result = matches!(msg, Msg::PromptResult { .. });
        let cmds = update(&mut state, msg);
        exec(cmds, &mut client, &mut prompt_ids).await;
        if is_question {
            let rows = draw(&state);
            let all = rows.join("\n");
            assert!(all.contains("Scope"), "header in modal: {all}");
            assert!(all.contains("1) MVP"), "numbered option: {all}");
            assert!(
                rows[STRIP].contains("waiting on you"),
                "halted strip: {}",
                rows[STRIP]
            );
            let cmds = press(&mut state, KeyCode::Char('1'));
            assert!(matches!(
                &cmds[..],
                [Cmd::ReplyQuestion { selected, free_text: None, .. }, ..]
                if selected == &vec!["MVP".to_string()]
            ));
            exec(cmds, &mut client, &mut prompt_ids).await;
        }
        if is_result {
            break;
        }
    }

    assert_eq!(state.phase, Phase::Idle);
    let rows = draw(&state);
    assert!(
        rows.iter().any(|r| r.contains("all done via tui")),
        "assistant text rendered after the question resolved"
    );
}

#[tokio::test]
async fn deny_with_reason_reaches_engine() {
    let (mut client, mut reader) = start().await;
    let mut state = State::new(true, "m".into());
    let mut prompt_ids = VecDeque::new();

    type_prompt(&mut state, &mut client, &mut prompt_ids, "go").await;
    loop {
        let Some(msg) = translate(next(&mut reader).await, &mut prompt_ids) else {
            continue;
        };
        let is_ask = matches!(msg, Msg::PermissionRequest { .. });
        let is_result = matches!(msg, Msg::PromptResult { .. });
        let cmds = update(&mut state, msg);
        exec(cmds, &mut client, &mut prompt_ids).await;
        if is_ask {
            press(&mut state, KeyCode::Char('n'));
            for c in "wrong dir".chars() {
                press(&mut state, KeyCode::Char(c));
            }
            let cmds = press(&mut state, KeyCode::Enter);
            assert!(matches!(&cmds[..],
                [Cmd::ReplyPermission { allow: false, message: Some(m), .. }, ..] if m == "wrong dir"));
            exec(cmds, &mut client, &mut prompt_ids).await;
        }
        if is_result {
            break;
        }
    }

    assert_eq!(state.phase, Phase::Idle, "the turn ends after the deny");
    let rows = draw(&state);
    // The denied card is now spine-marked: a ⛔ glyph in the gutter, the name
    // no longer bracketed. "⛔" is width-2, so match the marker and the name
    // separately rather than an exact "⛔ bash" run.
    assert!(
        rows.iter().any(|r| r.contains('⛔') && r.contains("bash")),
        "denied tool card renders: {:#?}",
        state.transcript
    );
}
