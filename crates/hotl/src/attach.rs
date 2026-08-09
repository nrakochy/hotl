//! `hotl attach [id]` — connect to a detached session's unix socket and drive
//! it from your terminal (the human client of `hotl serve`). Renders the
//! session's output, forwards what you type (prompt when idle, steer mid-turn),
//! and answers parked permission asks. `Ctrl-D` detaches (the session lives on);
//! `/stop` shuts the session down. Bare `hotl attach` lists live sessions.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::session_server::{list_live, socket_exists, socket_path};

pub async fn attach_main(id: Option<&str>) -> i32 {
    let Some(id) = id else {
        return list_sessions();
    };
    if !socket_exists(id) {
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

async fn connect(id: &str) -> i32 {
    let stream = match UnixStream::connect(socket_path(id)).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hotl attach: could not connect to `{id}`: {e} (is it still running?)");
            return 1;
        }
    };
    let (read, mut write) = stream.into_split();
    // Authenticate with the per-session token before anything else (Vuln 1).
    match std::fs::read_to_string(crate::session_server::token_path(id)) {
        Ok(token) => {
            let mut frame = json!({"t": "auth", "token": token.trim()}).to_string();
            frame.push('\n');
            if write.write_all(frame.as_bytes()).await.is_err() {
                eprintln!("hotl attach: could not authenticate to `{id}`");
                return 1;
            }
        }
        Err(e) => {
            eprintln!("hotl attach: no session token for `{id}` ({e}); is it still running?");
            return 1;
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

    let mut server = BufReader::new(read).lines();
    // The prompt currently awaiting a y/N answer, if any.
    let mut pending_ask: Option<Parked> = None;
    let mut turn_running = false;

    loop {
        tokio::select! {
            frame = server.next_line() => match frame {
                Ok(Some(line)) => {
                    if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                        if render(&msg, &mut pending_ask, &mut turn_running) {
                            turn_running = true;
                        }
                    }
                }
                _ => { println!("\n(session closed)"); return 0; }
            },
            line = stdin_rx.recv() => {
                let Some(line) = line else { return detach(&mut write).await };
                match on_input(line.trim(), &mut write, &mut pending_ask, turn_running).await {
                    Input::Continue => {}
                    Input::StartedTurn => turn_running = true,
                    Input::Stop => return 0,
                }
            }
        }
    }
}

enum Input {
    Continue,
    StartedTurn,
    Stop,
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

async fn on_input(
    text: &str,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    pending_ask: &mut Option<Parked>,
    turn_running: bool,
) -> Input {
    match route(text, turn_running, pending_ask.as_ref()) {
        Action::Stop => {
            let _ = send(write, json!({"t": "shutdown"})).await;
            Input::Stop
        }
        Action::Nothing => Input::Continue,
        Action::AnswerAsk { parked, allow } => {
            *pending_ask = None;
            let method = match &parked.egress {
                Some(_) => "egress_reply",
                None => "ask_reply",
            };
            let _ = send(write, json!({"t": method, "id": parked.id, "allow": allow})).await;
            if let (Some(host), true) = (&parked.egress, allow) {
                eprintln!("{}", crate::session_server::paste_hint(host));
            }
            Input::Continue
        }
        // Expansion happens here, not in `route`: reading files is I/O, and
        // the transcript should show what the human typed.
        Action::Steer(text) => {
            let text = crate::setup::expand_file_refs(&text);
            let _ = send(write, json!({"t": "steer", "text": text})).await;
            Input::Continue
        }
        Action::Prompt(text) => {
            let text = crate::setup::expand_file_refs(&text);
            let _ = send(write, json!({"t": "prompt", "text": text})).await;
            Input::StartedTurn
        }
    }
}

/// Render a server frame; returns true if it implies a turn is now running.
fn render(msg: &Value, pending_ask: &mut Option<Parked>, turn_running: &mut bool) -> bool {
    match msg.get("t").and_then(Value::as_str).unwrap_or("") {
        "hello" => false,
        "ask" => {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                if let Some(why) = msg.get("protectedWhy").and_then(Value::as_str) {
                    eprintln!("⚠ PROTECTED PATH — {why}");
                }
                eprint!(
                    "allow {}? [y/N] ",
                    msg.get("summary").and_then(Value::as_str).unwrap_or("?")
                );
                let _ = std::io::Write::flush(&mut std::io::stderr());
                *pending_ask = Some(Parked { id, egress: None });
            }
            true
        }
        // Deliberately different wording and framing from the tool ask above:
        // a human who just approved `npm install` must not read this as a
        // duplicate and reflex-key it.
        "egress" => {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                let host = msg.get("host").and_then(Value::as_str).unwrap_or("?");
                eprintln!("\nnetwork: reaching \"{host}\" was not in the approved command");
                eprintln!("this host is not in [network].allow");
                eprint!("allow for this session? [y/N] ");
                let _ = std::io::Write::flush(&mut std::io::stderr());
                *pending_ask = Some(Parked {
                    id,
                    egress: Some(host.to_string()),
                });
            }
            true
        }
        "update" => {
            render_update(msg.get("update").unwrap_or(&Value::Null));
            true
        }
        "turn_done" => {
            *turn_running = false;
            let kind = msg
                .pointer("/outcome/kind")
                .and_then(Value::as_str)
                .unwrap_or("done");
            if kind != "done" {
                eprintln!("\n[turn ended: {kind}]");
            }
            eprint!("\n❯ ");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            false
        }
        _ => false,
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
            "tool_denied" => format!("· denied: {}", s("name")),
            "tool_auto_allowed" => format!("· auto-allowed {} (rule: {})", s("name"), s("rule")),
            "retrying" => format!("· retrying (attempt {}) — {}", n("attempt"), s("reason")),
            "fallback_model" => format!("· model fallback → {}", s("model")),
            "prompt_queued" => "· queued".to_string(),
            "compacted" => {
                if update.get("degraded").and_then(Value::as_bool) == Some(true) {
                    "(context compacted — degraded)".to_string()
                } else {
                    "(context compacted)".to_string()
                }
            }
            "todos_changed" => {
                let n = update
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                format!("· todos: {n} item(s)")
            }
            "mode_changed" => format!("· permission mode → {}", s("mode")),
            _ => return None,
        },
    )
}

fn render_update(update: &Value) {
    if update.get("type").and_then(Value::as_str) == Some("text_delta") {
        print!(
            "{}",
            update.get("text").and_then(Value::as_str).unwrap_or("")
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
        return;
    }
    if let Some(line) = update_line(update) {
        eprintln!("{line}");
    }
}

async fn detach(write: &mut tokio::net::unix::OwnedWriteHalf) -> i32 {
    let _ = send(write, json!({"t": "detach"})).await;
    println!("\n(detached — session still running; `hotl attach` to return)");
    0
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, frame: Value) -> std::io::Result<()> {
    let mut line = frame.to_string();
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await
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
                name: "read".into(),
                summary: "read ./x".into(),
            },
            EngineEvent::ToolDone {
                name: "read".into(),
                ok: true,
            },
            EngineEvent::ToolDenied {
                name: "write".into(),
            },
            EngineEvent::ToolAutoAllowed {
                name: "bash".into(),
                rule: "ls*".into(),
            },
            EngineEvent::Retrying {
                attempt: 1,
                reason: "429".into(),
            },
            EngineEvent::FallbackModel { model: "m2".into() },
            EngineEvent::PromptQueued,
            EngineEvent::Compacted { degraded: false },
            EngineEvent::TodosChanged { items: Vec::new() },
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
        let exempt = ["text_delta", "tool_done"];
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
            name: "read".into(),
            ok: false,
        })
        .unwrap();
        assert!(update_line(&failed).is_some(), "a tool error must be shown");
    }

    #[test]
    fn typed_lines_route_correctly() {
        let ask = Parked {
            id: 4,
            egress: None,
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
}
