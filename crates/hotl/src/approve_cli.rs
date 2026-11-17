//! `hotl approve` (0058 T9): answer a parked ask or question from a second
//! terminal, without attaching.
//!
//! A backgrounded session parks whatever it is waiting on until someone
//! attaches (`session_server.rs`). Attaching to answer one y/N takes over the
//! session and evicts whoever else was there; this connects, answers by id,
//! and leaves. It is the CLI half of `human_input`: a workflow that pauses
//! for a person is only useful if the person can answer from where they are.

use hotl_platform::Ipc as _;
use hotl_tui::client::{AcpClient, ServerMsg};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

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
    let answer = match parse(&args[1..]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("hotl approve: {e}\n{USAGE}");
            return 2;
        }
    };
    connect(&id, answer).await
}

#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Ask { id: u64, allow: bool },
    Question { id: u64, option: String },
}

impl Answer {
    fn id(&self) -> u64 {
        match self {
            Answer::Ask { id, .. } | Answer::Question { id, .. } => *id,
        }
    }

    fn answers(&self, parked: &ServerMsg) -> bool {
        match (self, parked) {
            (Answer::Ask { id, .. }, ServerMsg::PermissionRequest { req_id, .. })
            | (Answer::Question { id, .. }, ServerMsg::QuestionRequest { req_id, .. }) => {
                id == req_id
            }
            _ => false,
        }
    }
}

/// What the caller asked us to send, or `None` to just list.
fn parse(args: &[String]) -> Result<Option<Answer>, String> {
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
            Ok(Some(Answer::Ask { id, allow }))
        }
        Some("--question") => {
            let id = num(args.get(1), "--question")?;
            let option = args
                .get(2)
                .ok_or("`--question <id>` needs the option label to choose")?;
            Ok(Some(Answer::Question {
                id,
                option: option.clone(),
            }))
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

async fn connect(id: &str, answer: Option<Answer>) -> i32 {
    let stream = match hotl_platform::IPC.connect(id).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hotl approve: could not connect to `{id}`: {e} (is it still running?)");
            return 1;
        }
    };
    let (read, write) = tokio::io::split(stream);
    let token = match std::fs::read_to_string(crate::session_server::token_path(id)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("hotl approve: no session token for `{id}` ({e}); is it still running?");
            return 1;
        }
    };
    let mut client = AcpClient::new(write);
    let mut lines = BufReader::new(read).lines();
    match crate::attach::open(&mut client, &mut lines, id, token.trim()).await {
        crate::attach::Opened::Attached => {}
        crate::attach::Opened::Refused(e) => {
            eprintln!("hotl approve: `{id}` refused the connection: {e}");
            return 1;
        }
        crate::attach::Opened::Closed => {
            eprintln!("hotl approve: could not authenticate to `{id}`");
            return 1;
        }
    }

    // Attaching re-issues everything parked, which is both the listing and
    // the proof the id we are about to answer still exists.
    let mut parked: Vec<ServerMsg> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        let next = tokio::time::timeout_at(deadline, lines.next_line()).await;
        match next {
            Ok(Ok(Some(line))) => {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(
                    msg @ (ServerMsg::PermissionRequest { .. }
                    | ServerMsg::QuestionRequest { .. }
                    | ServerMsg::EgressRequest { .. }),
                ) = hotl_tui::client::decode(&msg)
                {
                    client.note_server_msg(&msg);
                    parked.push(msg);
                }
            }
            // Closed, or nothing more to read: the re-issue is done.
            _ => break,
        }
    }

    let Some(answer) = answer else {
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
        let _ = client.request("session/close", json!({})).await;
        return 0;
    };

    let want = answer.id();
    if !parked.iter().any(|p| answer.answers(p)) {
        eprintln!(
            "hotl approve: {id} has nothing parked with id {want} of that kind. Run `hotl approve \
             {id}` to see what it is waiting on."
        );
        let _ = client.request("session/close", json!({})).await;
        return 1;
    }
    let sent = match answer {
        Answer::Ask { id, allow } => client.reply_permission(id, allow, false, None).await,
        Answer::Question { id, option } => client.reply_question(id, vec![option], None).await,
    };
    if sent.is_err() {
        eprintln!("hotl approve: could not send the answer to `{id}`");
        return 1;
    }
    println!("answered {id} #{want}");
    let _ = client.request("session/close", json!({})).await;
    0
}

/// One parked item as the listing shows it.
fn describe(msg: &ServerMsg) -> String {
    match msg {
        ServerMsg::PermissionRequest {
            req_id, summary, ..
        } => format!("#{req_id} ask — {summary} (--ask {req_id} allow|deny)"),
        ServerMsg::QuestionRequest { req_id, question } => {
            let options: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
            let what = if question.header.is_empty() {
                question.prompt.clone()
            } else {
                format!("{}: {}", question.header, question.prompt)
            };
            format!(
                "#{req_id} question — {what} (--question {req_id} <{}>)",
                options.join("|")
            )
        }
        ServerMsg::EgressRequest { req_id, host } => {
            format!("#{req_id} egress — {host} (attach to answer)")
        }
        ServerMsg::Update(_) | ServerMsg::Response { .. } => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_the_answer_and_refuses_anything_else() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parse(&s(&[])).unwrap(), None);
        assert_eq!(
            parse(&s(&["--ask", "3", "allow"])).unwrap(),
            Some(Answer::Ask { id: 3, allow: true })
        );
        assert_eq!(
            parse(&s(&["--ask", "3", "deny"])).unwrap(),
            Some(Answer::Ask {
                id: 3,
                allow: false
            })
        );
        assert_eq!(
            parse(&s(&["--question", "7", "ship"])).unwrap(),
            Some(Answer::Question {
                id: 7,
                option: "ship".into()
            })
        );
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

    fn decoded(msg: Value) -> ServerMsg {
        hotl_tui::client::decode(&msg).expect("a parked request")
    }

    #[test]
    fn the_listing_names_the_flag_that_answers_each_item() {
        let ask = decoded(
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/request_permission",
            "params": {"sessionId": "s", "toolCall": {"toolCallId": "t", "title": "write Makefile"}, "options": []}}),
        );
        let line = describe(&ask);
        assert!(line.contains("#2 ask — write Makefile"), "{line}");
        assert!(line.contains("--ask 2 allow|deny"), "{line}");

        let q = decoded(
            json!({"jsonrpc": "2.0", "id": 5, "method": "elicitation/create",
            "params": {"message": "Ship?: the release", "requestedSchema": {"type": "object",
                "properties": {"answer": {"type": "string", "enum": ["ship", "redo"]}}}}}),
        );
        let line = describe(&q);
        assert!(line.contains("#5 question — Ship?: the release"), "{line}");
        assert!(line.contains("--question 5 <ship|redo>"), "{line}");

        let e = decoded(
            json!({"jsonrpc": "2.0", "id": 9, "method": "session/request_permission",
            "params": {"sessionId": "s", "toolCall": {"toolCallId": "e", "_meta": {"hotl.dev/egress": {"host": "example.com"}}}, "options": []}}),
        );
        let line = describe(&e);
        assert!(
            line.contains("example.com") && line.contains("attach"),
            "{line}"
        );
    }

    #[test]
    fn an_ask_answer_never_answers_an_egress_prompt_or_a_question() {
        let egress = ServerMsg::EgressRequest {
            req_id: 4,
            host: "example.com".into(),
        };
        let ask = Answer::Ask { id: 4, allow: true };
        assert!(!ask.answers(&egress));
        let question = ServerMsg::QuestionRequest {
            req_id: 4,
            question: hotl_tools::ask::Question {
                header: String::new(),
                prompt: "p".into(),
                options: Vec::new(),
                multi: false,
            },
        };
        assert!(!ask.answers(&question));
        assert!(Answer::Question {
            id: 4,
            option: "x".into()
        }
        .answers(&question));
    }
}
