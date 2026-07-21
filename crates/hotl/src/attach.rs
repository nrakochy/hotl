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
    println!("attached to {id} — type to prompt · Ctrl-D detaches · /stop ends the session");
    let (read, mut write) = stream.into_split();

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
    // The id of an ask currently awaiting a y/N answer, if any.
    let mut pending_ask: Option<u64> = None;
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

/// Route one typed line: answer a pending ask, steer a running turn, start a
/// new turn, or handle `/stop`.
async fn on_input(
    text: &str,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    pending_ask: &mut Option<u64>,
    turn_running: bool,
) -> Input {
    if text == "/stop" {
        let _ = send(write, json!({"t": "shutdown"})).await;
        return Input::Stop;
    }
    if text.is_empty() {
        return Input::Continue;
    }
    if let Some(id) = pending_ask.take() {
        let allow = matches!(text, "y" | "Y" | "yes");
        let _ = send(write, json!({"t": "ask_reply", "id": id, "allow": allow})).await;
        return Input::Continue;
    }
    let text = crate::setup::expand_file_refs(text);
    if turn_running {
        let _ = send(write, json!({"t": "steer", "text": text})).await;
        Input::Continue
    } else {
        let _ = send(write, json!({"t": "prompt", "text": text})).await;
        Input::StartedTurn
    }
}

/// Render a server frame; returns true if it implies a turn is now running.
fn render(msg: &Value, pending_ask: &mut Option<u64>, turn_running: &mut bool) -> bool {
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
                *pending_ask = Some(id);
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

fn render_update(update: &Value) {
    match update.get("type").and_then(Value::as_str).unwrap_or("") {
        "text_delta" => {
            print!(
                "{}",
                update.get("text").and_then(Value::as_str).unwrap_or("")
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        "tool_start" => eprintln!(
            "\n· {}",
            update.get("summary").and_then(Value::as_str).unwrap_or("")
        ),
        "tool_done" => {
            if update.get("ok").and_then(Value::as_bool) == Some(false) {
                eprintln!("  (tool error — fed back to the model)");
            }
        }
        "compacted" => eprintln!("(context compacted)"),
        _ => {}
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
