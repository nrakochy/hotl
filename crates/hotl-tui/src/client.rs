//! ACP client codec: JSONL requests out, decoded server messages in. Pure
//! framing — the runtime owns the sockets and the select loop.
//!
//! `read_line` framing is safe here: the server emits `serde_json::to_string`
//! output, which escapes all control characters including newlines. (The Pi
//! U+2028 caveat applies to Node's readline splitting on Unicode line
//! separators, not to byte-linewise framing of serde output.)

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// One decoded server→client line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    /// The `params.update` object of a `session/update` notification.
    Update(Value),
    PermissionRequest {
        req_id: u64,
        summary: String,
        protected_why: Option<String>,
    },
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

    pub async fn reply_permission(&mut self, req_id: u64, allow: bool, message: Option<String>) {
        let mut result = json!({"allow": allow});
        if let Some(m) = message {
            result["message"] = json!(m);
        }
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
        }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, BufReader};

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

    #[tokio::test]
    async fn reply_permission_shape_matches_server_contract() {
        let (mut read, write) = tokio::io::duplex(4096);
        let mut client = AcpClient::new(write);
        client.reply_permission(7, true, None).await;
        client
            .reply_permission(8, false, Some("wrong dir".into()))
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
            json!({"jsonrpc": "2.0", "id": 7, "result": {"allow": true}})
        );
        assert_eq!(
            lines[1],
            json!({"jsonrpc": "2.0", "id": 8, "result": {"allow": false, "message": "wrong dir"}})
        );
    }
}
