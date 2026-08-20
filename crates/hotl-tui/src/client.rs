//! ACP client codec: JSONL requests out, decoded server messages in. Pure
//! framing — the runtime owns the sockets and the select loop.
//!
//! `read_line` framing is safe here: the server emits `serde_json::to_string`
//! output, which escapes all control characters including newlines. (The Pi
//! U+2028 caveat applies to Node's readline splitting on Unicode line
//! separators, not to byte-linewise framing of serde output.)

use hotl_tools::ask::{Question, QuestionOption};

use crate::app::{Cmd, DiffLine, DiffOp, Msg};
use serde_json::{json, Value};
use std::collections::VecDeque;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// A server-sent skill roster as `(name, description)` pairs — the `skills`
/// array of an `initialize` result or of a `config_reloaded` update.
///
/// Accepts the object shape `[{"name":…,"description":…}]` and the legacy bare
/// string shape `["name"]`, so a newer client against an older engine keeps
/// `/<skill>` dispatch and simply shows no descriptions. One parser for the
/// handshake and the reload notification, so a reloaded roster is decoded
/// exactly like the one it replaces.
pub fn parse_skills(from: &Value) -> Vec<(String, String)> {
    parse_roster(from, "skills")
}

/// The saved-workflow roster (0044) — the `workflows` array beside `skills`,
/// same shape, same two senders. Absent on an older engine: no `/<name>`.
pub fn parse_workflows(from: &Value) -> Vec<(String, String)> {
    parse_roster(from, "workflows")
}

fn parse_roster(from: &Value, key: &str) -> Vec<(String, String)> {
    from.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| match v {
                    Value::String(name) => Some((name.clone(), String::new())),
                    Value::Object(_) => v.get("name").and_then(Value::as_str).map(|name| {
                        let description = v
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        (name.to_string(), description.to_string())
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One decoded server→client line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    /// The `params.update` object of a `session/update` notification.
    Update(Value),
    PermissionRequest {
        req_id: u64,
        summary: String,
        protected_why: Option<String>,
        /// The proposed change, when the server sent one. Absent today for
        /// every ask — the engine's ask carries no tool input to diff (RQ-2,
        /// see `hotl::diffgen::for_tool`) — so this decodes to empty.
        diff: Vec<DiffLine>,
    },
    /// A `session/request_question` reverse-request (`ask_user`, tier-1
    /// gap #4) — NOT a permission gate; answering it never authorizes a tool.
    QuestionRequest { req_id: u64, question: Question },
    /// A `session/request_egress` reverse-request (plan 0026): a subprocess
    /// reached a host outside `[network].allow`, and no human-approved call in
    /// flight had shown it. IS a grant — answering `y` opens the socket — but
    /// it grants only this host, only this session.
    EgressRequest { req_id: u64, host: String },
    /// A reply to one of our requests (including the prompt result).
    Response {
        id: u64,
        result: Result<Value, String>,
    },
}

pub struct AcpClient<W: AsyncWrite + Unpin> {
    writer: W,
    next_id: u64,
}

impl<W: AsyncWrite + Unpin> AcpClient<W> {
    pub fn new(writer: W) -> Self {
        AcpClient { writer, next_id: 0 }
    }

    /// Send a request; returns the id used so the caller can match the reply.
    pub async fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if !params.is_null() {
            msg["params"] = params;
        }
        self.send(&msg).await;
        id
    }

    pub async fn reply_permission(
        &mut self,
        req_id: u64,
        allow: bool,
        secret_reads: bool,
        message: Option<String>,
    ) {
        let mut result = json!({"allow": allow});
        // Plan 0022: only ever sent when set, so the ordinary allow stays
        // byte-identical to the pre-0022 result an ACP client already parses.
        if secret_reads {
            result["secretReads"] = json!(true);
        }
        if let Some(m) = message {
            result["message"] = json!(m);
        }
        self.send(&json!({"jsonrpc": "2.0", "id": req_id, "result": result}))
            .await;
    }

    /// Answer a `session/request_egress` (plan 0026). Two answers, both
    /// scoped to this session; hotl never writes `config.toml`, so a
    /// permanent grant stays a deliberate edit.
    pub async fn reply_egress(&mut self, req_id: u64, allow: bool) {
        self.send(&json!({"jsonrpc": "2.0", "id": req_id, "result": {"allow": allow}}))
            .await;
    }

    /// Answer a `session/request_question`: exactly one of `selected`
    /// (labels) or `free_text` is set by the caller (`app::Cmd::ReplyQuestion`
    /// only ever produces one or the other).
    pub async fn reply_question(
        &mut self,
        req_id: u64,
        selected: Vec<String>,
        free_text: Option<String>,
    ) {
        let result = match free_text {
            Some(text) => json!({"freeText": text}),
            None => json!({"selected": selected}),
        };
        self.send(&json!({"jsonrpc": "2.0", "id": req_id, "result": result}))
            .await;
    }

    async fn send(&mut self, msg: &Value) {
        let mut line = msg.to_string();
        line.push('\n');
        let _ = self.writer.write_all(line.as_bytes()).await;
        let _ = self.writer.flush().await;
    }
}

/// Next decodable server message; malformed or unknown lines are skipped, not
/// fatal. `None` = EOF (the server hung up).
pub async fn read_server_msg<R: AsyncBufRead + Unpin>(r: &mut R) -> Option<ServerMsg> {
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(decoded) = decode(&msg) {
            return Some(decoded);
        }
    }
}

/// `params.diff` rows, or empty when the server sent none. Unknown ops are
/// dropped rather than guessed — a newer server's row type must not render as
/// an unlabelled line in an approval card.
fn decode_diff(v: Option<&Value>) -> Vec<DiffLine> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|row| {
            Some(DiffLine {
                op: DiffOp::from_wire(row.get("op").and_then(Value::as_str)?)?,
                text: row
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

fn decode(msg: &Value) -> Option<ServerMsg> {
    match msg.get("method").and_then(Value::as_str) {
        Some("session/update") => Some(ServerMsg::Update(msg.pointer("/params/update")?.clone())),
        Some("session/request_permission") => Some(ServerMsg::PermissionRequest {
            req_id: msg.get("id").and_then(Value::as_u64)?,
            summary: msg
                .pointer("/params/summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            protected_why: msg
                .pointer("/params/protectedWhy")
                .and_then(Value::as_str)
                .map(String::from),
            diff: decode_diff(msg.pointer("/params/diff")),
        }),
        Some("session/request_egress") => Some(ServerMsg::EgressRequest {
            req_id: msg.get("id").and_then(Value::as_u64)?,
            host: msg
                .pointer("/params/host")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        Some("session/request_question") => {
            let options = msg
                .pointer("/params/options")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|o| QuestionOption {
                            label: o
                                .get("label")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            description: o
                                .get("description")
                                .and_then(Value::as_str)
                                .map(String::from),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(ServerMsg::QuestionRequest {
                req_id: msg.get("id").and_then(Value::as_u64)?,
                question: Question {
                    header: msg
                        .pointer("/params/header")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    prompt: msg
                        .pointer("/params/prompt")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    options,
                    multi: msg
                        .pointer("/params/multi")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
            })
        }
        Some(_) => None,
        None => {
            let id = msg.get("id").and_then(Value::as_u64)?;
            let result = match msg.get("error") {
                Some(e) => Err(e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string()),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            Some(ServerMsg::Response { id, result })
        }
    }
}

/// One server message → an Elm message, or `None` for frames the loop only
/// needs as bookkeeping (a cancel/rename ack, or an accepted-steer ack, is
/// noise; a rejected steer is not — see the `Response` arm).
///
/// Lives here, not in the runtime, because it is pure — `ServerMsg` and `Msg`
/// are both this crate's types. `crates/hotl/tests/tui_e2e.rs` used to keep a
/// hand-copy that had already drifted from the real one (§7); both now call
/// this.
pub fn translate(
    msg: ServerMsg,
    prompt_ids: &mut VecDeque<u64>,
    steer_ids: &mut VecDeque<u64>,
) -> Option<Msg> {
    match msg {
        ServerMsg::Update(v) => Some(Msg::Update(v)),
        ServerMsg::PermissionRequest {
            req_id,
            summary,
            protected_why,
            diff,
        } => Some(Msg::PermissionRequest {
            req_id,
            summary,
            protected_why,
            diff,
        }),
        ServerMsg::QuestionRequest { req_id, question } => {
            Some(Msg::QuestionRequest { req_id, question })
        }
        ServerMsg::EgressRequest { req_id, host } => Some(Msg::EgressRequest { req_id, host }),
        ServerMsg::Response { id, result } => {
            if let Some(pos) = prompt_ids.iter().position(|&p| p == id) {
                prompt_ids.remove(pos);
                return Some(prompt_result_msg(result));
            }
            let pos = steer_ids.iter().position(|&p| p == id)?;
            steer_ids.remove(pos);
            // An ack is still noise; a rejection is not — the transcript
            // otherwise keeps claiming the steer is queued forever.
            result.err().map(|why| Msg::SteerRejected { why })
        }
    }
}

/// A prompt reply as a `Msg`. The outcome's human-readable payload lives under
/// a different key per kind, so all four are searched — reading only
/// `/outcome/text` silently drops the reason a doom-loop or failure-budget
/// turn ended.
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
                undo: v.get("undoStatus").cloned().unwrap_or(Value::Null),
            }
        }
        Err(e) => Msg::PromptResult {
            outcome_kind: "error".into(),
            outcome_text: Some(e),
            usage: Value::Null,
            undo: Value::Null,
        },
    }
}

/// Execute the wire-bound half of a `Cmd`, returning the commands it did not
/// handle — the terminal-bound ones (title, `$EDITOR`, history, quit) the
/// runtime owns. Shared by `hotl`'s run loop and the e2e harness so neither
/// can test against a divergent copy.
pub async fn exec_wire_cmd<W: AsyncWrite + Unpin>(
    cmd: Cmd,
    client: &mut AcpClient<W>,
    prompt_ids: &mut VecDeque<u64>,
    steer_ids: &mut VecDeque<u64>,
) -> Option<Cmd> {
    match cmd {
        Cmd::SendPrompt(p) => {
            prompt_ids.push_back(client.request("session/prompt", prompt_params(&p)).await);
        }
        Cmd::SendSteer(p) => {
            steer_ids.push_back(client.request("session/steer", prompt_params(&p)).await);
        }
        Cmd::Cancel => {
            client.request("session/cancel", Value::Null).await;
        }
        // Cancel/rename acks are noise: `translate` only surfaces prompt
        // replies and rejected steers.
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
        Cmd::SetPlan(plan) => {
            client
                .request("session/set_plan", json!({"plan": plan}))
                .await;
        }
        Cmd::SetEffort(effort) => {
            let wire = effort.as_deref().unwrap_or("default");
            client
                .request("session/set_effort", json!({"effort": wire}))
                .await;
        }
        // The ack is noise like set_mode: the engine's `goal_changed`
        // broadcast is what corrects the optimistic update. `None` rides as
        // JSON null, which is the wire's "clear".
        Cmd::SetGoal(goal) => {
            client
                .request("session/set_goal", json!({"goal": goal}))
                .await;
        }
        // The ack is noise like rename/set_mode: the engine broadcasts
        // `config_reloaded` (or `config_reload_failed`), and that is what the
        // client acts on — no id-plumbing in the runtime's loop.
        Cmd::ReloadConfig => {
            client.request("session/reload_config", Value::Null).await;
        }
        // The id is discarded: the report comes back as a `context_report`
        // broadcast, so there is nothing to correlate it against.
        Cmd::RequestContext => {
            client.request("session/context", json!({})).await;
        }
        Cmd::RequestWorkflows => {
            client.request("session/workflows", json!({})).await;
        }
        Cmd::ReplyPermission {
            req_id,
            allow,
            secret_reads,
            message,
        } => {
            client
                .reply_permission(req_id, allow, secret_reads, message)
                .await
        }
        Cmd::ReplyEgress { req_id, allow } => client.reply_egress(req_id, allow).await,
        Cmd::ReplyQuestion {
            req_id,
            selected,
            free_text,
        } => client.reply_question(req_id, selected, free_text).await,
        // Not ours: the runtime owns the terminal and the history file.
        other => return Some(other),
    }
    None
}

/// `session/prompt` / `session/steer` params. `images` carries only entries
/// the runtime seam encoded (`data: Some`) and is omitted entirely when
/// empty, so a text-only frame stays byte-identical to the pre-image
/// protocol — an older server never sees a key it does not know. Unencoded
/// entries are dropped, not asserted: the e2e harness legitimately drives
/// this without the runtime seam, exactly as it already skips `@[file]`
/// expansion.
fn prompt_params(p: &crate::paste::PromptPayload) -> Value {
    let mut params = json!({"text": p.text});
    let images: Vec<Value> = p
        .images
        .iter()
        .filter_map(|i| {
            i.data
                .as_ref()
                .map(|d| json!({"media_type": i.media_type, "data": d}))
        })
        .collect();
    if !images.is_empty() {
        params["images"] = json!(images);
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::io::{AsyncReadExt, BufReader};

    /// The e2e harness used to keep a hand-copy of this mapping, and the copy
    /// had already drifted: it read only `/outcome/text`, so `doom_loop` and
    /// `tool_failure_budget` outcomes lost their payload. One implementation,
    /// called by both the runtime loop and the test.
    #[test]
    fn translate_finds_outcome_text_for_every_outcome_shape() {
        for (key, kind) in [
            ("text", "done"),
            ("message", "error"),
            ("pattern", "doom_loop"),
            ("tool", "tool_failure_budget"),
        ] {
            let mut ids = VecDeque::from([7]);
            let mut steer_ids = VecDeque::new();
            let msg = ServerMsg::Response {
                id: 7,
                result: Ok(json!({"outcome": {"kind": kind, key: "payload"}})),
            };
            let Some(Msg::PromptResult {
                outcome_kind,
                outcome_text,
                ..
            }) = translate(msg, &mut ids, &mut steer_ids)
            else {
                panic!("not a prompt result")
            };
            assert_eq!(outcome_kind, kind);
            assert_eq!(outcome_text.as_deref(), Some("payload"), "key `{key}`");
        }
    }

    /// Acks for steer/cancel/rename are noise — only prompt replies become
    /// messages, which is what keeps the transcript from sprouting phantom
    /// turn results.
    #[test]
    fn translate_ignores_responses_that_are_not_prompt_replies() {
        let mut ids = VecDeque::from([7]);
        let mut steer_ids = VecDeque::new();
        let ack = ServerMsg::Response {
            id: 99,
            result: Ok(json!({"ok": true})),
        };
        assert!(translate(ack, &mut ids, &mut steer_ids).is_none());
        assert_eq!(ids.len(), 1, "an unrelated ack consumes no prompt id");
    }

    #[test]
    fn a_rejected_steer_surfaces_instead_of_vanishing() {
        let mut prompt_ids = VecDeque::new();
        let mut steer_ids = VecDeque::from([7]);
        let msg = ServerMsg::Response {
            id: 7,
            result: Err("images[0] is empty".into()),
        };
        let Some(Msg::SteerRejected { why }) = translate(msg, &mut prompt_ids, &mut steer_ids)
        else {
            panic!("a steer error must become a message");
        };
        assert!(why.contains("images[0]"));
        assert!(steer_ids.is_empty(), "the id is consumed either way");
    }

    #[test]
    fn an_accepted_steer_stays_noise() {
        let mut prompt_ids = VecDeque::new();
        let mut steer_ids = VecDeque::from([7]);
        let msg = ServerMsg::Response {
            id: 7,
            result: Ok(Value::Null),
        };
        assert!(translate(msg, &mut prompt_ids, &mut steer_ids).is_none());
        assert!(steer_ids.is_empty(), "the id is consumed either way");
    }

    #[tokio::test]
    async fn request_writes_jsonl_with_incrementing_ids() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        assert_eq!(client.request("initialize", Value::Null).await, 1);
        assert_eq!(
            client
                .request("session/prompt", json!({"text": "go"}))
                .await,
            2
        );
        drop(client);
        let mut out = String::new();
        read.read_to_string(&mut out).await.unwrap();
        let lines: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"})
        );
        assert_eq!(
            lines[1],
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"text": "go"}})
        );
    }

    #[tokio::test]
    async fn read_decodes_update_permission_and_response() {
        let (client, mut server) = tokio::io::duplex(4096);
        let feed = concat!(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"schemaVersion":1,"sessionId":"s","update":{"type":"text_delta","text":"hi"}}}"#,
            "\n",
            "this is not json\n",
            r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s","summary":"run bash","protectedWhy":"prod"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"result":{"outcome":{"kind":"done"}}}"#,
            "\n",
        );
        tokio::io::AsyncWriteExt::write_all(&mut server, feed.as_bytes())
            .await
            .unwrap();
        drop(server);
        let mut r = BufReader::new(client);
        assert_eq!(
            read_server_msg(&mut r).await,
            Some(ServerMsg::Update(
                json!({"type": "text_delta", "text": "hi"})
            ))
        );
        assert_eq!(
            read_server_msg(&mut r).await,
            Some(ServerMsg::PermissionRequest {
                req_id: 7,
                summary: "run bash".into(),
                protected_why: Some("prod".into()),
                diff: Vec::new(),
            }),
            "malformed line is skipped, not fatal"
        );
        assert_eq!(
            read_server_msg(&mut r).await,
            Some(ServerMsg::Response {
                id: 2,
                result: Ok(json!({"outcome": {"kind": "done"}}))
            })
        );
        assert_eq!(read_server_msg(&mut r).await, None, "EOF");
    }

    /// The client half of the diff pipeline. Nothing sends `params.diff` yet
    /// (RQ-2), so this pins the decode against a hand-built frame — including
    /// that an absent diff is empty, not a decode failure.
    #[test]
    fn a_permission_request_decodes_its_diff_rows() {
        let msg = json!({
            "method": "session/request_permission",
            "id": 3,
            "params": {"summary": "edit ./x", "diff": [
                {"op": "ctx", "text": "keep"},
                {"op": "del", "text": "was"},
                {"op": "add", "text": "now"},
                {"op": "sideways", "text": "from a newer server"},
            ]},
        });
        let Some(ServerMsg::PermissionRequest { diff, .. }) = decode(&msg) else {
            panic!("not a permission request");
        };
        assert_eq!(
            diff,
            vec![
                DiffLine {
                    op: DiffOp::Ctx,
                    text: "keep".into()
                },
                DiffLine {
                    op: DiffOp::Del,
                    text: "was".into()
                },
                DiffLine {
                    op: DiffOp::Add,
                    text: "now".into()
                },
            ],
            "an unknown op is dropped, never rendered unlabelled"
        );

        let bare = json!({"method": "session/request_permission", "id": 3,
                          "params": {"summary": "edit ./x"}});
        let Some(ServerMsg::PermissionRequest { diff, .. }) = decode(&bare) else {
            panic!("not a permission request");
        };
        assert!(diff.is_empty(), "no diff field decodes to no rows");
    }

    #[tokio::test]
    async fn read_decodes_an_egress_request() {
        let (client, mut server) = tokio::io::duplex(4096);
        let feed = concat!(
            r#"{"jsonrpc":"2.0","id":12,"method":"session/request_egress","params":{"sessionId":"s","host":"registry.npmjs.org"}}"#,
            "\n",
        );
        tokio::io::AsyncWriteExt::write_all(&mut server, feed.as_bytes())
            .await
            .unwrap();
        drop(server);
        let mut r = BufReader::new(client);
        assert_eq!(
            read_server_msg(&mut r).await,
            Some(ServerMsg::EgressRequest {
                req_id: 12,
                host: "registry.npmjs.org".into(),
            })
        );
    }

    #[tokio::test]
    async fn read_decodes_a_question_request() {
        let (client, mut server) = tokio::io::duplex(4096);
        let feed = concat!(
            r#"{"jsonrpc":"2.0","id":9,"method":"session/request_question","params":{"sessionId":"s","header":"Scope","prompt":"How far?","options":[{"label":"MVP"},{"label":"Full","description":"everything"}],"multi":false}}"#,
            "\n",
        );
        tokio::io::AsyncWriteExt::write_all(&mut server, feed.as_bytes())
            .await
            .unwrap();
        drop(server);
        let mut r = BufReader::new(client);
        assert_eq!(
            read_server_msg(&mut r).await,
            Some(ServerMsg::QuestionRequest {
                req_id: 9,
                question: Question {
                    header: "Scope".into(),
                    prompt: "How far?".into(),
                    options: vec![
                        QuestionOption {
                            label: "MVP".into(),
                            description: None,
                        },
                        QuestionOption {
                            label: "Full".into(),
                            description: Some("everything".into()),
                        },
                    ],
                    multi: false,
                },
            })
        );
    }

    #[tokio::test]
    async fn reply_question_shape_matches_server_contract() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        client.reply_question(9, vec!["MVP".into()], None).await;
        client
            .reply_question(10, Vec::new(), Some("other".into()))
            .await;
        drop(client);
        let mut out = String::new();
        read.read_to_string(&mut out).await.unwrap();
        let lines: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            json!({"jsonrpc": "2.0", "id": 9, "result": {"selected": ["MVP"]}})
        );
        assert_eq!(
            lines[1],
            json!({"jsonrpc": "2.0", "id": 10, "result": {"freeText": "other"}})
        );
    }

    #[tokio::test]
    async fn reply_permission_shape_matches_server_contract() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        client.reply_permission(7, true, false, None).await;
        client
            .reply_permission(8, false, false, Some("wrong dir".into()))
            .await;
        // Plan 0022: the grant rides the same result.
        client.reply_permission(9, true, true, None).await;
        drop(client);
        let mut out = String::new();
        read.read_to_string(&mut out).await.unwrap();
        let lines: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            json!({"jsonrpc": "2.0", "id": 7, "result": {"allow": true}})
        );
        assert_eq!(
            lines[1],
            json!({"jsonrpc": "2.0", "id": 8, "result": {"allow": false, "message": "wrong dir"}})
        );
        // `secretReads` is present only when taken, so an ordinary allow stays
        // byte-identical for every ACP client that has never heard of it.
        assert_eq!(
            lines[2],
            json!({"jsonrpc": "2.0", "id": 9, "result": {"allow": true, "secretReads": true}})
        );
    }

    /// The wire-compat pin: a text-only payload serializes byte-identically
    /// to the pre-image protocol (no `images` key at all), encoded entries
    /// ride, unencoded entries (harness runs without the runtime seam) drop.
    #[test]
    fn prompt_params_omit_images_when_empty_and_serialize_encoded_ones() {
        use crate::paste::{ImageAttachment, PromptPayload};
        let text_only = PromptPayload::text_only("hi".into());
        assert_eq!(prompt_params(&text_only).to_string(), r#"{"text":"hi"}"#);

        let mixed = PromptPayload {
            text: "see [Image #1] and [Image #2]".into(),
            images: vec![
                ImageAttachment {
                    marker: "[Image #1]".into(),
                    path: "/a/1.png".into(),
                    media_type: "image/png".into(),
                    data: Some("aW1n".into()),
                },
                ImageAttachment {
                    marker: "[Image #2]".into(),
                    path: "/a/2.png".into(),
                    media_type: "image/png".into(),
                    data: None,
                },
            ],
        };
        let params = prompt_params(&mixed);
        assert_eq!(
            params["images"],
            json!([{"media_type": "image/png", "data": "aW1n"}])
        );
    }

    #[test]
    fn skills_parse_from_the_object_shape_with_descriptions() {
        let hello = json!({"skills": [
            {"name": "review", "description": "review a pull request"},
            {"name": "bare"},
        ]});
        assert_eq!(
            parse_skills(&hello),
            vec![
                ("review".to_string(), "review a pull request".to_string()),
                ("bare".to_string(), String::new()),
            ]
        );
    }

    /// A newer TUI against an older engine degrades to names-only rather
    /// than losing `/<skill>` dispatch entirely.
    #[test]
    fn skills_still_parse_from_the_legacy_bare_string_shape() {
        let hello = json!({"skills": ["review", "acme:deploy"]});
        assert_eq!(
            parse_skills(&hello),
            vec![
                ("review".to_string(), String::new()),
                ("acme:deploy".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn workflows_parse_from_the_same_object_shape() {
        let hello = json!({"skills": [], "workflows": [
            {"name": "review-changes", "description": "review then verify"}
        ]});
        assert_eq!(
            parse_workflows(&hello),
            vec![(
                "review-changes".to_string(),
                "review then verify".to_string()
            )]
        );
        assert!(parse_workflows(&json!({"skills": ["x"]})).is_empty());
    }

    #[test]
    fn a_missing_skills_field_yields_nothing() {
        assert!(parse_skills(&json!({})).is_empty());
    }

    /// The handshake result and the `config_reloaded` update carry the same
    /// `skills` shape, so one parser serves both — a reloaded roster decodes
    /// exactly like the one it replaces.
    #[test]
    fn the_reload_update_and_the_handshake_share_one_skill_parser() {
        let update = json!({"type": "config_reloaded", "skills": [{"name": "run", "description": "launch the app"}]});
        assert_eq!(
            parse_skills(&update),
            vec![("run".to_string(), "launch the app".to_string())]
        );
    }

    #[tokio::test]
    async fn reload_config_is_a_bare_wire_request() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        let mut ids = VecDeque::new();
        let mut steer_ids = VecDeque::new();
        assert!(
            exec_wire_cmd(Cmd::ReloadConfig, &mut client, &mut ids, &mut steer_ids)
                .await
                .is_none(),
            "the wire half handles it; nothing returns to the runtime"
        );
        assert!(
            ids.is_empty() && steer_ids.is_empty(),
            "a reload is neither a prompt nor a steer — it must park no id"
        );
        drop(client);
        let mut out = String::new();
        read.read_to_string(&mut out).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(out.trim()).unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/reload_config"})
        );
    }

    /// The settings half never leaves the core — the runtime owns the
    /// filesystem, so `exec_wire_cmd` must hand it straight back.
    #[tokio::test]
    async fn reload_settings_is_the_runtime_half() {
        let (_read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        let mut ids = VecDeque::new();
        let mut steer_ids = VecDeque::new();
        assert_eq!(
            exec_wire_cmd(Cmd::ReloadSettings, &mut client, &mut ids, &mut steer_ids).await,
            Some(Cmd::ReloadSettings)
        );
    }
}
