//! ACP client codec: JSONL requests out, decoded server messages in. Pure
//! framing — the runtime owns the sockets and the select loop.
//!
//! Line framing is safe and cancel-safe here: the server emits
//! `serde_json::to_string` output, which escapes all control characters
//! including newlines, and `FrameReader` keeps a partial line across drops. (The Pi
//! U+2028 caveat applies to Node's readline splitting on Unicode line
//! separators, not to byte-linewise framing of serde output.)

use hotl_tools::ask::{Question, QuestionOption};

use crate::app::{Cmd, DiffLine, DiffOp, Msg};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

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
        diff: Vec<DiffLine>,
    },
    /// An `elicitation/create` reverse-request (`ask_user`, tier-1
    /// gap #4) — NOT a permission gate; answering it never authorizes a tool.
    QuestionRequest { req_id: u64, question: Question },
    /// A network `session/request_permission` reverse-request (plan 0026): a subprocess
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
    session_id: Option<String>,
    multi_questions: std::collections::HashSet<u64>,
}

impl<W: AsyncWrite + Unpin> AcpClient<W> {
    pub fn new(writer: W) -> Self {
        AcpClient {
            writer,
            next_id: 0,
            session_id: None,
            multi_questions: std::collections::HashSet::new(),
        }
    }

    pub fn note_server_msg(&mut self, msg: &ServerMsg) {
        if let ServerMsg::QuestionRequest { req_id, question } = msg {
            if question.multi {
                self.multi_questions.insert(*req_id);
            }
        }
    }

    pub fn set_session_id(&mut self, id: impl Into<String>) {
        self.session_id = Some(id.into());
    }

    pub fn clear_questions(&mut self) {
        self.multi_questions.clear();
    }

    fn with_session(&self, mut params: Value) -> Value {
        let Some(id) = &self.session_id else {
            return params;
        };
        if params.is_null() {
            params = json!({});
        }
        if let Some(map) = params.as_object_mut() {
            map.entry("sessionId").or_insert_with(|| json!(id));
        }
        params
    }

    /// Send a request; returns the id used so the caller can match the reply.
    pub async fn request(&mut self, method: &str, params: Value) -> io::Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        let params = self.with_session(params);
        let mut msg = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if !params.is_null() {
            msg["params"] = params;
        }
        self.send(&msg).await?;
        Ok(id)
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        let mut msg = json!({"jsonrpc": "2.0", "method": method});
        if !params.is_null() {
            msg["params"] = params;
        }
        self.send(&msg).await
    }

    pub async fn reply_permission(
        &mut self,
        req_id: u64,
        allow: bool,
        secret_reads: bool,
        message: Option<String>,
    ) -> io::Result<()> {
        let option = match (allow, secret_reads) {
            (false, _) => "deny",
            (true, true) => "allow-secret-reads",
            (true, false) => "allow",
        };
        let mut result = json!({"outcome": {"outcome": "selected", "optionId": option}});
        if let Some(m) = message {
            result["_meta"] = json!({"hotl.dev/reply": {"message": m}});
        }
        self.send(&json!({"jsonrpc": "2.0", "id": req_id, "result": result}))
            .await
    }

    /// Answer a network `session/request_permission` (plan 0026). Two answers, both
    /// scoped to this session; hotl never writes `config.toml`, so a
    /// permanent grant stays a deliberate edit.
    pub async fn reply_egress(&mut self, req_id: u64, allow: bool) -> io::Result<()> {
        let option = if allow { "allow" } else { "deny" };
        self.send(&json!({"jsonrpc": "2.0", "id": req_id, "result": {"outcome": {"outcome": "selected", "optionId": option}}}))
            .await
    }

    /// Answer an `elicitation/create` question: exactly one of `selected`
    /// (labels) or `free_text` is set by the caller (`app::Cmd::ReplyQuestion`
    /// only ever produces one or the other).
    pub async fn reply_question(
        &mut self,
        req_id: u64,
        selected: Vec<String>,
        free_text: Option<String>,
    ) -> io::Result<()> {
        let multi = self.multi_questions.remove(&req_id);
        let result = match (free_text, selected.len()) {
            (Some(text), _) => json!({"action": "accept", "content": {"other": text}}),
            (None, 0) => json!({"action": "decline"}),
            (None, _) if multi => json!({"action": "accept", "content": {"answer": selected}}),
            (None, _) => json!({"action": "accept", "content": {"answer": selected[0]}}),
        };
        self.send(&json!({"jsonrpc": "2.0", "id": req_id, "result": result}))
            .await
    }

    /// A write failure on the duplex/stdio pipe means the server is gone and
    /// the reader would EOF next: it *is* the hangup (0061 decision 13), so
    /// it propagates rather than being swallowed.
    async fn send(&mut self, msg: &Value) -> io::Result<()> {
        let mut line = msg.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await
    }
}

/// Line framing that survives being dropped mid-line. `read_line` keeps its
/// partial bytes in the future; the run loop's `select!` drops that future
/// on every tick, so under backlog a frame's head vanished and its tail was
/// skipped as malformed. Here the partial lives in the struct.
pub struct FrameReader<R> {
    inner: tokio::io::BufReader<R>,
    partial: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        FrameReader {
            inner: tokio::io::BufReader::new(inner),
            partial: Vec::new(),
        }
    }

    /// One whole line without its `\n`, or `None` at EOF. Cancel-safe: bytes
    /// move from the buffer into `partial` only after `fill_buf` returned
    /// them, and `partial` outlives any one call.
    async fn next_line(&mut self) -> Option<String> {
        loop {
            let buf = self.inner.fill_buf().await.ok()?;
            if buf.is_empty() {
                // `read_line` parity: an unterminated last line still counts.
                if self.partial.is_empty() {
                    return None;
                }
                let line = std::mem::take(&mut self.partial);
                return Some(String::from_utf8_lossy(&line).into_owned());
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    self.partial.extend_from_slice(&buf[..pos]);
                    self.inner.consume(pos + 1);
                    let line = std::mem::take(&mut self.partial);
                    return Some(String::from_utf8_lossy(&line).into_owned());
                }
                None => {
                    let n = buf.len();
                    self.partial.extend_from_slice(buf);
                    self.inner.consume(n);
                }
            }
        }
    }
}

/// Next decodable server message; malformed or unknown lines are skipped, not
/// fatal. `None` = EOF (the server hung up).
pub async fn read_server_msg<R: tokio::io::AsyncRead + Unpin>(
    r: &mut FrameReader<R>,
) -> Option<ServerMsg> {
    loop {
        let line = r.next_line().await?;
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

fn standard_update(kind: &str, update: &Value) -> Option<Value> {
    match kind {
        "current_mode_update" => {
            let mode = update.get("currentModeId").and_then(Value::as_str)?;
            Some(json!({"type": "mode_changed", "mode": mode}))
        }
        "usage_update" => {
            let used = update.get("used").and_then(Value::as_u64)?;
            Some(json!({"type": "context_used", "used": used}))
        }
        "notice" => {
            let title = update.get("title").and_then(Value::as_str)?;
            Some(json!({"type": "server_notice", "text": title}))
        }
        "config_option_update" => {
            if let Some(change) = update.pointer("/_meta/hotl.dev~1model") {
                let field = |k: &str| change.get(k).cloned().unwrap_or(Value::Null);
                return Some(json!({
                    "type": "model_changed",
                    "model": field("model"),
                    "previous": field("previous"),
                    "context_tokens": field("contextTokens"),
                    "context_window": field("contextWindow"),
                }));
            }
            let options = update.get("configOptions").and_then(Value::as_array)?;
            let current = |id: &str| {
                options
                    .iter()
                    .find(|o| o.get("id").and_then(Value::as_str) == Some(id))
                    .and_then(|o| o.get("currentValue"))
            };
            Some(json!({
                "type": "config_options",
                "plan": current("plan").and_then(Value::as_bool),
                "effort": current("effort").and_then(Value::as_str),
            }))
        }
        _ => None,
    }
}

fn decode_question(params: &Value) -> Option<Question> {
    if let Some(q) = params.pointer("/_meta/hotl.dev~1question") {
        let text = |key: &str| q.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let options = q
            .get("options")
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
        return Some(Question {
            header: text("header"),
            prompt: text("prompt"),
            options,
            multi: q.get("multi").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    let answer = params.pointer("/requestedSchema/properties/answer");
    let labels = answer
        .and_then(|a| a.get("enum").or_else(|| a.pointer("/items/enum")))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|label| QuestionOption {
                    label: label.to_string(),
                    description: None,
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Question {
        header: String::new(),
        prompt: params.get("message").and_then(Value::as_str)?.to_string(),
        options: labels,
        multi: answer.is_some_and(|a| a.get("type").and_then(Value::as_str) == Some("array")),
    })
}

pub fn decode(msg: &Value) -> Option<ServerMsg> {
    match msg.get("method").and_then(Value::as_str) {
        Some("session/update") => {
            let update = msg.pointer("/params/update")?;
            if let Some(frame) = update.pointer("/_meta/hotl.dev~1frame") {
                return Some(ServerMsg::Update(frame.clone()));
            }
            let kind = update.get("sessionUpdate").and_then(Value::as_str)?;
            standard_update(kind, update).map(ServerMsg::Update)
        }
        Some("_hotl/session/update") => {
            Some(ServerMsg::Update(msg.pointer("/params/update")?.clone()))
        }
        Some("session/request_permission") => {
            let req_id = msg.get("id").and_then(Value::as_u64)?;
            let call = msg.pointer("/params/toolCall")?;
            if let Some(egress) = call.pointer("/_meta/hotl.dev~1egress") {
                return Some(ServerMsg::EgressRequest {
                    req_id,
                    host: egress
                        .get("host")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                });
            }
            let ask = call.pointer("/_meta/hotl.dev~1ask");
            let text = |key: &str| ask.and_then(|a| a.get(key)).and_then(Value::as_str);
            Some(ServerMsg::PermissionRequest {
                req_id,
                summary: text("summary")
                    .or_else(|| call.get("title").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string(),
                protected_why: text("protectedWhy").map(String::from),
                diff: decode_diff(ask.and_then(|a| a.get("diff"))),
            })
        }
        Some("elicitation/create") => {
            let req_id = msg.get("id").and_then(Value::as_u64)?;
            let question = decode_question(msg.get("params")?)?;
            Some(ServerMsg::QuestionRequest { req_id, question })
        }
        Some(_) => None,
        None => {
            let id = msg.get("id").and_then(Value::as_u64)?;
            let result = match msg.get("error") {
                Some(e) => match e.pointer("/data/_meta/hotl.dev~1turn") {
                    Some(turn) => Ok(json!({"_meta": {"hotl.dev/turn": turn}})),
                    None if e.pointer("/data/type").and_then(Value::as_str)
                        == Some("config_reload_failed") =>
                    {
                        Ok(e.get("data").cloned().unwrap_or(Value::Null))
                    }
                    None => Err(e
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("error")
                        .to_string()),
                },
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
    model_ids: &mut VecDeque<u64>,
    remember_ids: &mut VecDeque<u64>,
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
            if let Some(pos) = model_ids.iter().position(|&p| p == id) {
                model_ids.remove(pos);
                let result =
                    result.map(|v| v.pointer("/_meta/hotl.dev~1model").cloned().unwrap_or(v));
                return Some(Msg::ModelAck { result });
            }
            if let Some(pos) = remember_ids.iter().position(|&p| p == id) {
                remember_ids.remove(pos);
                if let Some(update) = result.as_ref().ok().and_then(report_update) {
                    return Some(Msg::Update(update));
                }
                return Some(Msg::RememberAck {
                    result: result.map(|_| ()),
                });
            }
            if let Some(update) = result.as_ref().ok().and_then(report_update) {
                return Some(Msg::Update(update));
            }
            let pos = steer_ids.iter().position(|&p| p == id)?;
            steer_ids.remove(pos);
            // An ack is still noise; a rejection is not — the transcript
            // otherwise keeps claiming the steer is queued forever.
            result.err().map(|why| Msg::SteerRejected { why })
        }
    }
}

const REPORT_KINDS: [&str; 6] = [
    "memory_report",
    "context_report",
    "workflows_report",
    "jobs_report",
    "children_report",
    "config_reload_failed",
];

fn report_update(result: &Value) -> Option<Value> {
    let kind = result.get("type").and_then(Value::as_str)?;
    if REPORT_KINDS.contains(&kind) {
        return Some(result.clone());
    }
    if kind != "config_reloaded" {
        return None;
    }
    let field = |k: &str| result.get(k).cloned().unwrap_or(Value::Null);
    Some(json!({
        "type": "config_reloaded",
        "model": field("model"),
        "mode": field("mode"),
        "plan": field("plan"),
        "context_window": field("contextWindow"),
        "skills": field("skills"),
        "workflows": field("workflows"),
        "warnings": field("warnings"),
    }))
}

/// A prompt reply as a `Msg`. The outcome's human-readable payload lives under
/// a different key per kind, so all four are searched — reading only
/// `/outcome/text` silently drops the reason a doom-loop or failure-budget
/// turn ended.
fn prompt_result_msg(result: Result<Value, String>) -> Msg {
    match result {
        Ok(v) => {
            let turn = v.pointer("/_meta/hotl.dev~1turn").unwrap_or(&Value::Null);
            let text = ["text", "message", "pattern", "tool"]
                .iter()
                .find_map(|k| {
                    turn.pointer(&format!("/outcome/{k}"))
                        .and_then(Value::as_str)
                })
                .map(String::from);
            let kind = turn
                .pointer("/outcome/kind")
                .and_then(Value::as_str)
                .or_else(|| {
                    v.get("stopReason")
                        .and_then(Value::as_str)
                        .map(kind_of_stop_reason)
                })
                .unwrap_or("error");
            Msg::PromptResult {
                outcome_kind: kind.to_string(),
                outcome_text: text,
                usage: turn.get("usage").cloned().unwrap_or(Value::Null),
                // The runtime owns the clock (0061 T9).
                finished_at: None,
            }
        }
        Err(e) => Msg::PromptResult {
            outcome_kind: "error".into(),
            outcome_text: Some(e),
            usage: Value::Null,
            finished_at: None,
        },
    }
}

fn kind_of_stop_reason(reason: &str) -> &'static str {
    match reason {
        "end_turn" => "done",
        "cancelled" => "cancelled",
        "max_turn_requests" => "turn_limit",
        "refusal" => "refused",
        _ => "error",
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
    model_ids: &mut VecDeque<u64>,
    remember_ids: &mut VecDeque<u64>,
) -> io::Result<Option<Cmd>> {
    match cmd {
        Cmd::SendPrompt(p) => {
            prompt_ids.push_back(client.request("session/prompt", prompt_params(&p)).await?);
        }
        Cmd::SendSteer(p) => {
            steer_ids.push_back(
                client
                    .request("_hotl/session/steer", steer_params(&p))
                    .await?,
            );
        }
        Cmd::Cancel => {
            client.clear_questions();
            let params = client.with_session(json!({}));
            client.notify("session/cancel", params).await?;
        }
        // Cancel/rename acks are noise: `translate` only surfaces prompt
        // replies and rejected steers.
        Cmd::Rename(name) => {
            client
                .request("_hotl/session/rename", json!({"name": name}))
                .await?;
        }
        Cmd::SetColor(color) => {
            client
                .request("_hotl/session/set_color", json!({"color": color}))
                .await?;
        }
        Cmd::SetMode(mode) => {
            client
                .request("session/set_mode", json!({"modeId": mode}))
                .await?;
        }
        Cmd::SetPlan(plan) => {
            client
                .request(
                    "session/set_config_option",
                    json!({"configId": "plan", "type": "boolean", "value": plan}),
                )
                .await?;
        }
        Cmd::SetEffort(effort) => {
            let wire = effort.as_deref().unwrap_or("default");
            client
                .request(
                    "session/set_config_option",
                    json!({"configId": "effort", "value": wire}),
                )
                .await?;
        }
        // The ack is read (0067): queued-or-applied, warnings, or a refusal.
        Cmd::SetModel(spec) => {
            model_ids.push_back(
                client
                    .request(
                        "session/set_config_option",
                        json!({"configId": "model", "value": spec}),
                    )
                    .await?,
            );
        }
        // The ack carries information (the refusal text), unlike set_goal's.
        Cmd::Remember(note) => {
            let params = match note {
                Some(n) => json!({"note": n}),
                None => json!({}),
            };
            remember_ids.push_back(client.request("_hotl/session/remember", params).await?);
        }
        // The ack is noise like set_mode: the engine's `goal_changed`
        // broadcast is what corrects the optimistic update. `None` rides as
        // JSON null, which is the wire's "clear".
        Cmd::SetGoal(goal) => {
            client
                .request("_hotl/session/set_goal", json!({"goal": goal}))
                .await?;
        }
        Cmd::Aside(question) => {
            client
                .request("_hotl/session/aside", json!({"question": question}))
                .await?;
        }
        Cmd::Fold(focus) => {
            let params = match focus {
                Some(f) => json!({"focus": f}),
                None => json!({}),
            };
            client.request("_hotl/session/fold", params).await?;
        }
        // The reply carries the report; `translate` turns it into an update.
        Cmd::ReloadConfig => {
            client
                .request("_hotl/session/reload_config", Value::Null)
                .await?;
        }
        // The reply carries the report; `translate` turns it into an update.
        Cmd::RequestContext => {
            client.request("_hotl/session/context", json!({})).await?;
        }
        Cmd::RequestWorkflows => {
            client.request("_hotl/session/workflows", json!({})).await?;
        }
        Cmd::RequestJobs => {
            client.request("_hotl/session/jobs", json!({})).await?;
        }
        Cmd::StopJob(job) => {
            client
                .request("_hotl/session/jobs/stop", json!({"job": job}))
                .await?;
        }
        Cmd::RequestChildren => {
            client.request("_hotl/session/children", json!({})).await?;
        }
        Cmd::StopChild(child) => {
            client
                .request("_hotl/session/children/stop", json!({"child": child}))
                .await?;
        }
        Cmd::ReplyPermission {
            req_id,
            allow,
            secret_reads,
            message,
        } => {
            client
                .reply_permission(req_id, allow, secret_reads, message)
                .await?
        }
        Cmd::ReplyEgress { req_id, allow } => client.reply_egress(req_id, allow).await?,
        Cmd::ReplyQuestion {
            req_id,
            selected,
            free_text,
        } => client.reply_question(req_id, selected, free_text).await?,
        // Not ours: the runtime owns the terminal and the history file.
        other => return Ok(Some(other)),
    }
    Ok(None)
}

fn prompt_params(p: &crate::paste::PromptPayload) -> Value {
    let mut blocks = vec![json!({"type": "text", "text": p.text})];
    blocks.extend(p.images.iter().filter_map(|i| {
        i.data
            .as_ref()
            .map(|d| json!({"type": "image", "mimeType": i.media_type, "data": d}))
    }));
    json!({"prompt": blocks})
}

/// `_hotl/session/steer` params. `images` carries only entries
/// the runtime seam encoded (`data: Some`) and is omitted entirely when
/// empty, so a text-only frame stays byte-identical to the pre-image
/// protocol — an older server never sees a key it does not know. Unencoded
/// entries are dropped, not asserted: the e2e harness legitimately drives
/// this without the runtime seam, exactly as it already skips `@[file]`
/// expansion.
fn steer_params(p: &crate::paste::PromptPayload) -> Value {
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
    use tokio::io::AsyncReadExt;

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
                result: Ok(
                    json!({"stopReason": "end_turn", "_meta": {"hotl.dev/turn": {"outcome": {"kind": kind, key: "payload"}}}}),
                ),
            };
            let Some(Msg::PromptResult {
                outcome_kind,
                outcome_text,
                ..
            }) = translate(
                msg,
                &mut ids,
                &mut steer_ids,
                &mut VecDeque::new(),
                &mut VecDeque::new(),
            )
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
        assert!(translate(
            ack,
            &mut ids,
            &mut steer_ids,
            &mut VecDeque::new(),
            &mut VecDeque::new()
        )
        .is_none());
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
        let Some(Msg::SteerRejected { why }) = translate(
            msg,
            &mut prompt_ids,
            &mut steer_ids,
            &mut VecDeque::new(),
            &mut VecDeque::new(),
        ) else {
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
        assert!(translate(
            msg,
            &mut prompt_ids,
            &mut steer_ids,
            &mut VecDeque::new(),
            &mut VecDeque::new()
        )
        .is_none());
        assert!(steer_ids.is_empty(), "the id is consumed either way");
    }

    /// A writer that always fails, for the hangup path.
    struct BrokenWriter;

    impl AsyncWrite for BrokenWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone")))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// 0061 T27: a write failure on the pipe means the server is gone and the
    /// reader would EOF next — it *is* the hangup, and swallowing it left the
    /// loop waiting for an answer to a message that never left.
    #[tokio::test]
    async fn a_failed_write_surfaces_as_an_io_error() {
        let mut client = AcpClient::new(BrokenWriter);
        let err = client
            .request("initialize", Value::Null)
            .await
            .expect_err("a broken pipe must not be swallowed");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);

        let mut ids = VecDeque::new();
        let mut steer_ids = VecDeque::new();
        let err = exec_wire_cmd(
            Cmd::ReloadConfig,
            &mut client,
            &mut ids,
            &mut steer_ids,
            &mut VecDeque::new(),
            &mut VecDeque::new(),
        )
        .await
        .expect_err("the failure propagates through the dispatcher");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn request_writes_jsonl_with_incrementing_ids() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        assert_eq!(client.request("initialize", Value::Null).await.unwrap(), 1);
        assert_eq!(
            client
                .request("session/prompt", json!({"text": "go"}))
                .await
                .unwrap(),
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

    /// The run loop drops this future whenever a tick or a key wins its
    /// `select!`. A half-read line must survive the drop — before 0062 T2 it
    /// was lost with the future and the tail parsed as garbage.
    #[test]
    fn a_standard_session_update_is_skipped_for_now() {
        let msg: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}"#,
        )
        .unwrap();
        assert_eq!(decode(&msg), None);
    }

    #[test]
    fn a_standard_update_with_a_frame_decodes_as_that_frame() {
        let msg: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"ls","kind":"execute","status":"in_progress","_meta":{"hotl.dev/frame":{"type":"tool_start","id":"t1","name":"bash","summary":"ls"}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            decode(&msg),
            Some(ServerMsg::Update(
                json!({"type": "tool_start", "id": "t1", "name": "bash", "summary": "ls"})
            ))
        );
        let ext: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"_hotl/session/update","params":{"sessionId":"s","update":{"type":"goal_changed","goal":null}}}"#,
        )
        .unwrap();
        assert_eq!(
            decode(&ext),
            Some(ServerMsg::Update(
                json!({"type": "goal_changed", "goal": null})
            ))
        );
        let old: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"type":"text_delta","text":"hi"}}}"#,
        )
        .unwrap();
        assert_eq!(decode(&old), None);
    }

    #[test]
    fn the_mode_and_option_updates_decode_into_the_old_kinds() {
        let mode: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"current_mode_update","currentModeId":"bypass"}}}"#,
        )
        .unwrap();
        assert_eq!(
            decode(&mode),
            Some(ServerMsg::Update(
                json!({"type": "mode_changed", "mode": "bypass"})
            ))
        );
        let options: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"config_option_update","configOptions":[
                {"id":"model","type":"select","currentValue":"m","options":[]},
                {"id":"effort","type":"select","currentValue":"max","options":[]},
                {"id":"plan","type":"boolean","currentValue":true}]}}}"#,
        )
        .unwrap();
        assert_eq!(
            decode(&options),
            Some(ServerMsg::Update(
                json!({"type": "config_options", "plan": true, "effort": "max"})
            ))
        );
    }

    #[test]
    fn a_usage_update_decodes_into_context_used() {
        let msg: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"usage_update","used":12000,"size":200000}}}"#,
        )
        .unwrap();
        assert_eq!(
            decode(&msg),
            Some(ServerMsg::Update(
                json!({"type": "context_used", "used": 12000})
            ))
        );
    }

    #[test]
    fn a_model_change_update_decodes_into_model_changed() {
        let msg: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"config_option_update","configOptions":[],"_meta":{"hotl.dev/model":{"model":"a/b","previous":"a/c","contextTokens":7,"contextWindow":9}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            decode(&msg),
            Some(ServerMsg::Update(json!({
                "type": "model_changed", "model": "a/b", "previous": "a/c",
                "context_tokens": 7, "context_window": 9
            })))
        );
    }

    #[test]
    fn the_model_ack_reads_the_extension_meta() {
        let result = json!({"configOptions": [], "_meta": {"hotl.dev/model": {"applied": false, "model": "x"}}});
        let msg = translate(
            ServerMsg::Response {
                id: 5,
                result: Ok(result),
            },
            &mut VecDeque::new(),
            &mut VecDeque::new(),
            &mut VecDeque::from([5]),
            &mut VecDeque::new(),
        );
        assert_eq!(
            msg,
            Some(Msg::ModelAck {
                result: Ok(json!({"applied": false, "model": "x"}))
            })
        );
    }

    #[tokio::test]
    async fn a_dropped_read_keeps_the_partial_line() {
        let (client, mut server) = tokio::io::duplex(4096);
        let mut r = FrameReader::new(client);
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"},"_meta":{"hotl.dev/frame":{"type":"text_delta","text":"hi"}}}}}"#;
        let (head, tail) = line.split_at(40);
        tokio::io::AsyncWriteExt::write_all(&mut server, head.as_bytes())
            .await
            .unwrap();
        // Poll the read once (it takes the head), then let the other branch
        // win and drop it — what the run loop does thirty times a second.
        tokio::select! {
            biased;
            _ = read_server_msg(&mut r) => panic!("no whole line has arrived"),
            _ = tokio::task::yield_now() => {}
        }
        tokio::io::AsyncWriteExt::write_all(&mut server, format!("{tail}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(
            read_server_msg(&mut r).await,
            Some(ServerMsg::Update(
                json!({"type": "text_delta", "text": "hi"})
            )),
            "the head was lost with the dropped future"
        );
    }

    #[tokio::test]
    async fn read_decodes_update_permission_and_response() {
        let (client, mut server) = tokio::io::duplex(4096);
        let feed = concat!(
            r#"{"jsonrpc":"2.0","method":"_hotl/session/update","params":{"sessionId":"s","update":{"type":"text_delta","text":"hi"}}}"#,
            "\n",
            "this is not json\n",
            r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"t1","title":"run bash","_meta":{"hotl.dev/ask":{"summary":"run bash","protectedWhy":"prod","diff":[]}}},"options":[]}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"result":{"outcome":{"kind":"done"}}}"#,
            "\n",
        );
        tokio::io::AsyncWriteExt::write_all(&mut server, feed.as_bytes())
            .await
            .unwrap();
        drop(server);
        let mut r = FrameReader::new(client);
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

    #[test]
    fn a_permission_request_decodes_its_diff_rows() {
        let msg = json!({
            "method": "session/request_permission",
            "id": 3,
            "params": {"toolCall": {"toolCallId": "t", "title": "edit ./x", "_meta": {"hotl.dev/ask": {"summary": "edit ./x", "diff": [
                {"op": "ctx", "text": "keep"},
                {"op": "del", "text": "was"},
                {"op": "add", "text": "now"},
                {"op": "sideways", "text": "from a newer server"},
            ]}}}},
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
                          "params": {"toolCall": {"toolCallId": "t", "title": "edit ./x — why"}}});
        assert_eq!(
            decode(&bare),
            Some(ServerMsg::PermissionRequest {
                req_id: 3,
                summary: "edit ./x — why".into(),
                protected_why: None,
                diff: Vec::new(),
            }),
            "a plain request decodes its title as the summary, with no rows"
        );
    }

    #[tokio::test]
    async fn read_decodes_an_egress_request() {
        let (client, mut server) = tokio::io::duplex(4096);
        let feed = concat!(
            r#"{"jsonrpc":"2.0","id":12,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"egress-12","kind":"fetch","title":"Network access to registry.npmjs.org","_meta":{"hotl.dev/egress":{"host":"registry.npmjs.org"}}},"options":[]}}"#,
            "\n",
        );
        tokio::io::AsyncWriteExt::write_all(&mut server, feed.as_bytes())
            .await
            .unwrap();
        drop(server);
        let mut r = FrameReader::new(client);
        assert_eq!(
            read_server_msg(&mut r).await,
            Some(ServerMsg::EgressRequest {
                req_id: 12,
                host: "registry.npmjs.org".into(),
            })
        );
    }

    #[test]
    fn a_tool_call_named_like_an_egress_is_still_a_tool_prompt() {
        let msg: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"egress-1","kind":"fetch","title":"web_fetch https://evil.example","rawInput":{"host":"registry.npmjs.org"}},"options":[]}}"#,
        )
        .unwrap();
        assert_eq!(
            decode(&msg),
            Some(ServerMsg::PermissionRequest {
                req_id: 3,
                summary: "web_fetch https://evil.example".into(),
                protected_why: None,
                diff: Vec::new(),
            })
        );
    }

    #[tokio::test]
    async fn read_decodes_a_question_request() {
        let (client, mut server) = tokio::io::duplex(4096);
        let feed = concat!(
            r#"{"jsonrpc":"2.0","id":9,"method":"elicitation/create","params":{"sessionId":"s","mode":"form","message":"Scope: How far?","requestedSchema":{"type":"object","properties":{}},"_meta":{"hotl.dev/question":{"header":"Scope","prompt":"How far?","options":[{"label":"MVP"},{"label":"Full","description":"everything"}],"multi":false}}}}"#,
            "\n",
        );
        tokio::io::AsyncWriteExt::write_all(&mut server, feed.as_bytes())
            .await
            .unwrap();
        drop(server);
        let mut r = FrameReader::new(client);
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
    async fn read_decodes_a_plain_elicitation_from_its_schema() {
        let (client, mut server) = tokio::io::duplex(4096);
        let feed = concat!(
            r#"{"jsonrpc":"2.0","id":9,"method":"elicitation/create","params":{"sessionId":"s","mode":"form","message":"Scope: How far?","requestedSchema":{"type":"object","properties":{"answer":{"type":"array","items":{"type":"string","enum":["MVP","Full"]}}}}}}"#,
            "\n",
        );
        tokio::io::AsyncWriteExt::write_all(&mut server, feed.as_bytes())
            .await
            .unwrap();
        drop(server);
        let mut r = FrameReader::new(client);
        let Some(ServerMsg::QuestionRequest { question, .. }) = read_server_msg(&mut r).await
        else {
            panic!("a question request");
        };
        assert_eq!(question.prompt, "Scope: How far?");
        assert_eq!(question.options.len(), 2);
        assert!(question.multi);
    }

    #[tokio::test]
    async fn a_multi_question_answers_with_a_list_even_for_one_label() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        let ask = |multi| Question {
            header: "h".into(),
            prompt: "p".into(),
            options: Vec::new(),
            multi,
        };
        client.note_server_msg(&ServerMsg::QuestionRequest {
            req_id: 1,
            question: ask(true),
        });
        client.note_server_msg(&ServerMsg::QuestionRequest {
            req_id: 2,
            question: ask(false),
        });
        client
            .reply_question(1, vec!["MVP".into()], None)
            .await
            .unwrap();
        client
            .reply_question(2, vec!["MVP".into()], None)
            .await
            .unwrap();
        drop(client);
        let mut out = String::new();
        read.read_to_string(&mut out).await.unwrap();
        let lines: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["result"]["content"]["answer"], json!(["MVP"]));
        assert_eq!(lines[1]["result"]["content"]["answer"], json!("MVP"));
    }

    #[tokio::test]
    async fn reply_question_shape_matches_server_contract() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        client
            .reply_question(9, vec!["MVP".into()], None)
            .await
            .unwrap();
        client
            .reply_question(10, Vec::new(), Some("other".into()))
            .await
            .unwrap();
        client
            .reply_question(11, vec!["MVP".into()], None)
            .await
            .unwrap();
        client.reply_question(12, Vec::new(), None).await.unwrap();
        drop(client);
        let mut out = String::new();
        read.read_to_string(&mut out).await.unwrap();
        let lines: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            json!({"jsonrpc": "2.0", "id": 9, "result": {"action": "accept", "content": {"answer": "MVP"}}})
        );
        assert_eq!(
            lines[1],
            json!({"jsonrpc": "2.0", "id": 10, "result": {"action": "accept", "content": {"other": "other"}}})
        );
        assert_eq!(
            lines[2]["result"],
            json!({"action": "accept", "content": {"answer": "MVP"}})
        );
        assert_eq!(lines[3]["result"], json!({"action": "decline"}));
    }

    #[tokio::test]
    async fn reply_permission_shape_matches_server_contract() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        client.reply_permission(7, true, false, None).await.unwrap();
        client
            .reply_permission(8, false, false, Some("wrong dir".into()))
            .await
            .unwrap();
        client.reply_permission(9, true, true, None).await.unwrap();
        client.reply_egress(10, true).await.unwrap();
        client.reply_egress(11, false).await.unwrap();
        drop(client);
        let mut out = String::new();
        read.read_to_string(&mut out).await.unwrap();
        let lines: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let picked = |id: u64, option: &str| json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "selected", "optionId": option}}});
        assert_eq!(lines[0], picked(7, "allow"));
        assert_eq!(
            lines[1],
            json!({"jsonrpc": "2.0", "id": 8, "result": {"outcome": {"outcome": "selected", "optionId": "deny"}, "_meta": {"hotl.dev/reply": {"message": "wrong dir"}}}})
        );
        assert_eq!(lines[2], picked(9, "allow-secret-reads"));
        assert_eq!(lines[3], picked(10, "allow"));
        assert_eq!(lines[4], picked(11, "deny"));
    }

    /// The wire-compat pin: a text-only payload serializes byte-identically
    /// to the pre-image protocol (no `images` key at all), encoded entries
    /// ride, unencoded entries (harness runs without the runtime seam) drop.
    #[test]
    fn steer_params_omit_images_when_empty_and_serialize_encoded_ones() {
        use crate::paste::{ImageAttachment, PromptPayload};
        let text_only = PromptPayload::text_only("hi".into());
        assert_eq!(steer_params(&text_only).to_string(), r#"{"text":"hi"}"#);

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
        let params = steer_params(&mixed);
        assert_eq!(
            params["images"],
            json!([{"media_type": "image/png", "data": "aW1n"}])
        );
    }

    fn mixed_payload() -> crate::paste::PromptPayload {
        use crate::paste::{ImageAttachment, PromptPayload};
        PromptPayload {
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
        }
    }

    async fn written(cmd: Cmd) -> (Vec<Value>, VecDeque<u64>) {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        client.set_session_id("s1");
        let mut ids = VecDeque::new();
        exec_wire_cmd(
            cmd,
            &mut client,
            &mut ids,
            &mut VecDeque::new(),
            &mut VecDeque::new(),
            &mut VecDeque::new(),
        )
        .await
        .expect("the write succeeded");
        drop(client);
        let mut out = String::new();
        read.read_to_string(&mut out).await.unwrap();
        let lines = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        (lines, ids)
    }

    #[tokio::test]
    async fn steer_and_ext_requests_name_the_session() {
        let (lines, _) = written(Cmd::SendSteer(mixed_payload())).await;
        assert_eq!(lines[0]["method"], "_hotl/session/steer");
        assert_eq!(lines[0]["params"]["sessionId"], "s1");
        let (lines, _) = written(Cmd::RequestJobs).await;
        assert_eq!(lines[0]["params"]["sessionId"], "s1");
        let (lines, _) = written(Cmd::ReloadConfig).await;
        assert_eq!(lines[0]["params"], json!({"sessionId": "s1"}));
    }

    #[tokio::test]
    async fn cancel_forgets_open_multi_questions() {
        let (_read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        client.note_server_msg(&ServerMsg::QuestionRequest {
            req_id: 4,
            question: Question {
                header: String::new(),
                prompt: String::new(),
                options: vec![],
                multi: true,
            },
        });
        exec_wire_cmd(
            Cmd::Cancel,
            &mut client,
            &mut VecDeque::new(),
            &mut VecDeque::new(),
            &mut VecDeque::new(),
            &mut VecDeque::new(),
        )
        .await
        .unwrap();
        assert!(client.multi_questions.is_empty());
    }

    #[test]
    fn agent_message_chunk_decodes_as_text() {
        let msg = decode(&serde_json::from_str(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"},"_meta":{"hotl.dev/frame":{"type":"text_delta","text":"hi"}}}}}"#).unwrap());
        assert!(
            matches!(msg, Some(ServerMsg::Update(v)) if v["type"] == "text_delta" && v["text"] == "hi")
        );
    }

    #[test]
    fn notice_and_compaction_updates_decode_as_their_frames() {
        let frames = [
            json!({"type": "retrying", "attempt": 1, "max": 3}),
            json!({"type": "budget_notice", "pct": 80}),
            json!({"type": "fold_skipped", "reason": "r"}),
            json!({"type": "aside_failed", "question": "q", "reason": "r"}),
            json!({"type": "fallback_model", "model": "m"}),
            json!({"type": "compacting", "items": 3}),
            json!({"type": "compacted", "degraded": false}),
        ];
        for (i, frame) in frames.iter().enumerate() {
            let (kind, extra) = if i >= 5 {
                (
                    "compaction_update",
                    json!({"compactionId": "c", "status": "in_progress"}),
                )
            } else {
                ("notice", json!({"severity": "warning", "title": "t"}))
            };
            let mut update = extra;
            update["sessionUpdate"] = json!(kind);
            update["_meta"] = json!({"hotl.dev/frame": frame});
            let line = json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s", "update": update}});
            match decode(&line) {
                Some(ServerMsg::Update(v)) => assert_eq!(&v, frame),
                other => panic!("{kind} did not decode: {other:?}"),
            }
        }
    }

    #[test]
    fn an_unframed_notice_decodes_as_a_server_notice() {
        let msg = decode(
            &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s", "update": {"sessionUpdate": "notice", "severity": "warning", "title": "w"}}}),
        );
        assert!(
            matches!(msg, Some(ServerMsg::Update(v)) if v == json!({"type": "server_notice", "text": "w"}))
        );
    }

    #[tokio::test]
    async fn a_prompt_sends_content_blocks_with_the_session() {
        let (lines, ids) = written(Cmd::SendPrompt(mixed_payload())).await;
        assert_eq!(
            lines,
            [
                json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt", "params": {
                    "sessionId": "s1",
                    "prompt": [
                        {"type": "text", "text": "see [Image #1] and [Image #2]"},
                        {"type": "image", "mimeType": "image/png", "data": "aW1n"}
                    ]
                }})
            ]
        );
        assert_eq!(ids, [1]);
    }

    #[tokio::test]
    async fn cancel_is_a_notification_naming_the_session() {
        let (lines, ids) = written(Cmd::Cancel).await;
        assert_eq!(
            lines,
            [json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}})]
        );
        assert!(ids.is_empty());
    }

    #[test]
    fn an_error_turn_end_reads_like_the_old_error_result() {
        let msg = decode(&json!({"jsonrpc": "2.0", "id": 7, "error": {
            "code": -32603,
            "message": "authentication failed: bad key",
            "data": {"kind": "error", "message": "authentication failed: bad key",
                "_meta": {"hotl.dev/turn": {
                    "outcome": {"kind": "error", "message": "authentication failed: bad key"},
                    "usage": {"input_tokens": 3}
                }}}
        }}))
        .unwrap();
        let got = translate(
            msg,
            &mut VecDeque::from([7]),
            &mut VecDeque::new(),
            &mut VecDeque::new(),
            &mut VecDeque::new(),
        );
        assert_eq!(
            got,
            Some(Msg::PromptResult {
                outcome_kind: "error".into(),
                outcome_text: Some("authentication failed: bad key".into()),
                usage: json!({"input_tokens": 3}),
                finished_at: None,
            })
        );
    }

    #[test]
    fn a_done_reply_reads_its_usage_from_the_turn_meta() {
        let got = translate(
            ServerMsg::Response {
                id: 7,
                result: Ok(
                    json!({"stopReason": "end_turn", "_meta": {"hotl.dev/turn": {
                        "outcome": {"kind": "done", "text": "ok"},
                        "usage": {"input_tokens": 5}
                    }}}),
                ),
            },
            &mut VecDeque::from([7]),
            &mut VecDeque::new(),
            &mut VecDeque::new(),
            &mut VecDeque::new(),
        );
        assert_eq!(
            got,
            Some(Msg::PromptResult {
                outcome_kind: "done".into(),
                outcome_text: Some("ok".into()),
                usage: json!({"input_tokens": 5}),
                finished_at: None,
            })
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

    #[test]
    fn a_report_reply_becomes_the_update_the_view_reads() {
        let reply = ServerMsg::Response {
            id: 4,
            result: Ok(json!({"type": "jobs_report", "jobs": []})),
        };
        let Some(Msg::Update(v)) = translate(
            reply,
            &mut VecDeque::new(),
            &mut VecDeque::new(),
            &mut VecDeque::new(),
            &mut VecDeque::new(),
        ) else {
            panic!("a report reply is an update");
        };
        assert_eq!(v["type"], "jobs_report");
    }

    #[test]
    fn a_reload_reply_reads_like_the_old_broadcast() {
        let v = report_update(&json!({
            "type": "config_reloaded", "ok": true, "sessionId": "s", "model": "m2",
            "mode": "plan", "plan": false, "contextWindow": 900000,
            "skills": [], "workflows": [], "warnings": ["w"]
        }))
        .unwrap();
        assert_eq!(v["context_window"], 900000);
        assert_eq!(v["warnings"], json!(["w"]));
    }

    #[test]
    fn a_failed_reload_error_decodes_to_the_failure_report() {
        let line = json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32603,
            "message": "config reload failed: x",
            "data": {"type": "config_reload_failed", "reason": "x"}}});
        let Some(ServerMsg::Response { result: Ok(v), .. }) = decode(&line) else {
            panic!("the failure report rides the error");
        };
        assert_eq!(report_update(&v).unwrap()["reason"], "x");
    }

    #[tokio::test]
    async fn reload_config_is_a_bare_wire_request() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        let mut ids = VecDeque::new();
        let mut steer_ids = VecDeque::new();
        assert!(
            exec_wire_cmd(
                Cmd::ReloadConfig,
                &mut client,
                &mut ids,
                &mut steer_ids,
                &mut VecDeque::new(),
                &mut VecDeque::new(),
            )
            .await
            .expect("the write succeeded")
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
            json!({"jsonrpc": "2.0", "id": 1, "method": "_hotl/session/reload_config"})
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
            exec_wire_cmd(
                Cmd::ReloadSettings,
                &mut client,
                &mut ids,
                &mut steer_ids,
                &mut VecDeque::new(),
                &mut VecDeque::new(),
            )
            .await
            .expect("the write succeeded"),
            Some(Cmd::ReloadSettings)
        );
    }
}
