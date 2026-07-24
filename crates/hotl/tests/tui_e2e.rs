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
use hotl_tui::client::{read_server_msg, AcpClient, ServerMsg};
use hotl_tui::view::view;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::{json, Value};
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};

// The server module lives in the binary crate; pull it in directly.
#[path = "../src/acp.rs"]
#[allow(dead_code)]
mod acp;

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
                config: EngineConfig {
                    max_turns: 6,
                    ..Default::default()
                },
            }),
            name: None,
        })
    })
}

/// Spin up serve + client and complete the pre-TUI handshake.
async fn start() -> (Client, Reader) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server_io);
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), Vec::new()));
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

/// Mirror of the runtime's translate: server msg → Elm msg.
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
            let pos = prompt_ids.iter().position(|&p| p == id)?;
            prompt_ids.remove(pos);
            let v = result.expect("prompt result");
            Some(Msg::PromptResult {
                outcome_kind: v
                    .pointer("/outcome/kind")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string(),
                outcome_text: v
                    .pointer("/outcome/text")
                    .and_then(Value::as_str)
                    .map(String::from),
                usage: v.get("usage").cloned().unwrap_or(Value::Null),
            })
        }
    }
}

/// Mirror of the runtime's cmd executor for the wire-bound commands.
async fn exec(cmds: Vec<Cmd>, client: &mut Client, prompt_ids: &mut VecDeque<u64>) {
    for cmd in cmds {
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
                client
                    .request("session/rename", json!({"name": name}))
                    .await;
            }
            Cmd::SetMode(mode) => {
                client
                    .request("session/set_mode", json!({"mode": mode}))
                    .await;
            }
            Cmd::ReplyPermission {
                req_id,
                allow,
                message,
            } => client.reply_permission(req_id, allow, message).await,
            Cmd::OpenEditor(_) | Cmd::SetTitle(_) | Cmd::AppendHistory(_) | Cmd::Quit => {}
        }
    }
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
