//! `hotl approve` (0058 T9): answer a parked ask or question from a second
//! terminal, without attaching.
//!
//! A backgrounded session parks whatever it is waiting on until someone
//! attaches (`session_server.rs`). Attaching to answer one y/N takes over the
//! session and evicts whoever else was there; this connects, answers by id,
//! and leaves. It is the CLI half of `human_input`: a workflow that pauses
//! for a person is only useful if the person can answer from where they are.

use hotl_platform::Ipc as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const USAGE: &str = "usage:\n  \
     hotl approve <session>                       list what it is waiting on\n  \
     hotl approve <session> --ask <id> allow|deny  answer a permission ask\n  \
     hotl approve <session> --question <id> <option>\n                                               \
     answer an ask_user question";

pub async fn approve_main(args: &[String]) -> i32 {
    let Some(id) = args.first() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let Some(id) = resolve(id) else {
        eprintln!("hotl approve: no live session `{id}` (`hotl attach` lists them)");
        return 1;
    };
    let reply = match parse(&args[1..]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("hotl approve: {e}\n{USAGE}");
            return 2;
        }
    };
    connect(&id, reply).await
}

/// What the caller asked us to send, or `None` to just list.
fn parse(args: &[String]) -> Result<Option<Value>, String> {
    match args.first().map(String::as_str) {
        None => Ok(None),
        Some("--ask") => {
            let id = num(args.get(1), "--ask")?;
            let verdict = args
                .get(2)
                .map(String::as_str)
                .ok_or("`--ask <id>` needs `allow` or `deny`")?;
            let allow = match verdict {
                "allow" => true,
                "deny" => false,
                other => {
                    return Err(format!("`{other}` is not `allow` or `deny`"));
                }
            };
            Ok(Some(json!({"t": "ask_reply", "id": id, "allow": allow})))
        }
        Some("--question") => {
            let id = num(args.get(1), "--question")?;
            let option = args
                .get(2)
                .ok_or("`--question <id>` needs the option label to choose")?;
            Ok(Some(
                json!({"t": "question_reply", "id": id, "option": option}),
            ))
        }
        Some(other) => Err(format!("`{other}` is not an option")),
    }
}

fn num(raw: Option<&String>, flag: &str) -> Result<u64, String> {
    raw.ok_or_else(|| format!("`{flag}` needs the id from the listing"))?
        .parse()
        .map_err(|_| format!("`{flag}` takes a number — the id from the listing"))
}

/// A full session id, accepting a prefix the way `hotl attach` does.
fn resolve(id: &str) -> Option<String> {
    if hotl_platform::IPC.liveness(id) == hotl_platform::Liveness::Live {
        return Some(id.to_string());
    }
    crate::session_server::list_live()
        .into_iter()
        .find(|s| s.starts_with(id))
}

async fn connect(id: &str, reply: Option<Value>) -> i32 {
    let stream = match hotl_platform::IPC.connect(id).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hotl approve: could not connect to `{id}`: {e} (is it still running?)");
            return 1;
        }
    };
    let (read, mut write) = tokio::io::split(stream);
    let token = match std::fs::read_to_string(crate::session_server::token_path(id)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("hotl approve: no session token for `{id}` ({e}); is it still running?");
            return 1;
        }
    };
    let send = |v: &Value| -> String {
        let mut line = v.to_string();
        line.push('\n');
        line
    };
    if write
        .write_all(send(&json!({"t": "auth", "token": token.trim()})).as_bytes())
        .await
        .is_err()
    {
        eprintln!("hotl approve: could not authenticate to `{id}`");
        return 1;
    }

    // Attaching re-issues everything parked, which is both the listing and
    // the proof the id we are about to answer still exists.
    let mut lines = BufReader::new(read).lines();
    let mut parked: Vec<Value> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        let next = tokio::time::timeout_at(deadline, lines.next_line()).await;
        match next {
            Ok(Ok(Some(line))) => {
                if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                    if matches!(
                        msg.get("t").and_then(Value::as_str),
                        Some("ask" | "question" | "egress")
                    ) {
                        parked.push(msg);
                    }
                }
            }
            // Closed, or nothing more to read: the re-issue is done.
            _ => break,
        }
    }

    let Some(reply) = reply else {
        if parked.is_empty() {
            println!("{id} is not waiting on anything.");
        } else {
            println!("{id} is waiting on:");
            for p in &parked {
                println!("  {}", describe(p));
            }
        }
        // Detaching rather than dropping: the session must not think a human
        // is still here.
        let _ = write
            .write_all(send(&json!({"t": "detach"})).as_bytes())
            .await;
        return 0;
    };

    let want = reply["id"].as_u64();
    if !parked
        .iter()
        .any(|p| p.get("id").and_then(Value::as_u64) == want)
    {
        eprintln!(
            "hotl approve: {id} has nothing parked with id {}. Run `hotl approve {id}` to see \
             what it is waiting on.",
            want.unwrap_or(0)
        );
        let _ = write
            .write_all(send(&json!({"t": "detach"})).as_bytes())
            .await;
        return 1;
    }
    if write.write_all(send(&reply).as_bytes()).await.is_err() {
        eprintln!("hotl approve: could not send the answer to `{id}`");
        return 1;
    }
    println!("answered {id} #{}", want.unwrap_or(0));
    let _ = write
        .write_all(send(&json!({"t": "detach"})).as_bytes())
        .await;
    0
}

/// One parked item as the listing shows it.
fn describe(msg: &Value) -> String {
    let id = msg.get("id").and_then(Value::as_u64).unwrap_or(0);
    let text = |k: &str| msg.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match msg.get("t").and_then(Value::as_str) {
        Some("ask") => format!("#{id} ask — {} (--ask {id} allow|deny)", text("summary")),
        Some("question") => {
            let options: Vec<String> = msg
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o.get("label").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "#{id} question — {}: {} (--question {id} <{}>)",
                text("header"),
                text("prompt"),
                options.join("|")
            )
        }
        Some("egress") => format!("#{id} egress — {} (attach to answer)", text("host")),
        _ => format!("#{id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_builds_the_wire_frames_and_refuses_anything_else() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parse(&s(&[])).unwrap(), None);
        assert_eq!(
            parse(&s(&["--ask", "3", "allow"])).unwrap(),
            Some(json!({"t": "ask_reply", "id": 3, "allow": true}))
        );
        assert_eq!(
            parse(&s(&["--ask", "3", "deny"])).unwrap(),
            Some(json!({"t": "ask_reply", "id": 3, "allow": false}))
        );
        assert_eq!(
            parse(&s(&["--question", "7", "ship"])).unwrap(),
            Some(json!({"t": "question_reply", "id": 7, "option": "ship"}))
        );
        // A verdict that is neither is refused, never read as a grant.
        assert!(parse(&s(&["--ask", "3", "maybe"]))
            .unwrap_err()
            .contains("not `allow` or `deny`"));
        assert!(parse(&s(&["--ask", "x", "allow"]))
            .unwrap_err()
            .contains("takes a number"));
        assert!(parse(&s(&["--ask"])).unwrap_err().contains("the id"));
        assert!(parse(&s(&["--question", "7"]))
            .unwrap_err()
            .contains("option label"));
        assert!(parse(&s(&["--nope"]))
            .unwrap_err()
            .contains("not an option"));
    }

    #[test]
    fn the_listing_names_the_flag_that_answers_each_item() {
        let ask = describe(&json!({"t": "ask", "id": 2, "summary": "write Makefile"}));
        assert!(ask.contains("#2 ask — write Makefile"), "{ask}");
        assert!(ask.contains("--ask 2 allow|deny"), "{ask}");

        let q = describe(&json!({
            "t": "question", "id": 5, "header": "Ship?", "prompt": "the release",
            "options": [{"label": "ship"}, {"label": "redo"}]
        }));
        assert!(q.contains("#5 question — Ship?: the release"), "{q}");
        assert!(q.contains("--question 5 <ship|redo>"), "{q}");

        let e = describe(&json!({"t": "egress", "id": 9, "host": "example.com"}));
        assert!(e.contains("example.com") && e.contains("attach"), "{e}");
    }
}
