//! Newline-delimited JSON-RPC 2.0 over stdio (the MCP stdio transport).
//!
//! One reader task per connection routes responses to pending requests by
//! id, flips the staleness flag on `tools/list_changed`, and answers any
//! server→client request with "method not found" so a chatty server can't
//! wedge the pipe. A dead server surfaces as errors on pending requests —
//! never a harness crash.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

const REQUEST_TIMEOUT_SECS: u64 = 30;
pub const PROTOCOL_VERSION: &str = "2025-06-18";

type Pending = Mutex<HashMap<u64, oneshot::Sender<Value>>>;
type Writer = tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>;

pub struct Client {
    next_id: AtomicU64,
    pending: Pending,
    writer: Writer,
    /// Set by `notifications/tools/list_changed`; the tool re-lists on next use.
    pub tools_stale: AtomicBool,
    _child: Option<tokio::process::Child>,
}

#[derive(Debug, Clone)]
pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl Client {
    /// Spawn the server process and connect over its stdio.
    pub fn connect(command: &str, args: &[String]) -> Result<Arc<Self>, String> {
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not start `{command}`: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        Ok(Self::start(stdout, stdin, Some(child)))
    }

    /// Connect over arbitrary streams (tests use in-process duplex pipes).
    pub fn from_streams(
        read: impl AsyncRead + Send + Unpin + 'static,
        write: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Arc<Self> {
        Self::start(read, write, None)
    }

    fn start(
        read: impl AsyncRead + Send + Unpin + 'static,
        write: impl AsyncWrite + Send + Unpin + 'static,
        child: Option<tokio::process::Child>,
    ) -> Arc<Self> {
        let client = Arc::new(Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            writer: tokio::sync::Mutex::new(Box::new(write) as Box<_>),
            tools_stale: AtomicBool::new(false),
            _child: child,
        });
        // The reader holds only a Weak: when the last user drops the client,
        // the child's stdin drops with it (a server that exits on stdin EOF
        // sees it) and the reader task winds down instead of pinning both.
        tokio::spawn(reader_task(read, Arc::downgrade(&client)));
        client
    }

    async fn send(&self, msg: &Value) -> Result<(), String> {
        let mut line = msg.to_string();
        line.push('\n');
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes()).await.map_err(|e| format!("server pipe closed: {e}"))?;
        w.flush().await.map_err(|e| format!("server pipe closed: {e}"))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, tx);
        // Every early return (send failure, timeout) must remove the entry
        // again or it leaks forever; the guard makes that unskippable.
        let guard = PendingGuard { pending: &self.pending, id: Some(id) };
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let reply = tokio::time::timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS), rx)
            .await
            .map_err(|_| format!("`{method}` timed out after {REQUEST_TIMEOUT_SECS}s"))?
            .map_err(|_| "server disconnected".to_string())?;
        // A reply means the reader already removed the entry.
        guard.disarm();
        if let Some(err) = reply.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(format!("server error on `{method}`: {msg}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn notify(&self, method: &str) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method})).await
    }

    /// The MCP handshake: `initialize` request, then the initialized notice.
    pub async fn initialize(&self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "hotl", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
        self.notify("notifications/initialized").await
    }

    pub async fn list_tools(&self) -> Result<Vec<RemoteTool>, String> {
        self.tools_stale.store(false, Ordering::Relaxed);
        let result = self.request("tools/list", json!({})).await?;
        Ok(result
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .map(|t| RemoteTool {
                        name: str_field(t, "name"),
                        description: str_field(t, "description"),
                        input_schema: t
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or(json!({"type": "object"})),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Returns (joined text content, is_error).
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<(String, bool), String> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .map(|b| str_field(b, "text"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok((text, is_error))
    }
}

fn str_field(v: &Value, field: &str) -> String {
    v.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Removes its id from the pending map on drop unless disarmed — so no early
/// return out of `request` can leak the entry.
struct PendingGuard<'a> {
    pending: &'a Pending,
    id: Option<u64>,
}

impl PendingGuard<'_> {
    fn disarm(mut self) {
        self.id = None;
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
        }
    }
}

async fn reader_task(read: impl AsyncRead + Send + Unpin, client: Weak<Client>) {
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        // Upgrade per message: a Weak here means the last user is gone and
        // the task must not keep the client (and the child's stdin) alive.
        let Some(client) = client.upgrade() else { return };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let id = msg.get("id").and_then(Value::as_u64);
        let method = msg.get("method").and_then(Value::as_str);
        match (id, method) {
            // Response to one of our requests.
            (Some(id), None) => {
                if let Some(tx) =
                    client.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id)
                {
                    let _ = tx.send(msg);
                }
            }
            // Server→client request: refuse politely so nothing hangs.
            (Some(id), Some(_)) => {
                let _ = client
                    .send(&json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "hotl does not serve requests"},
                    }))
                    .await;
            }
            // Notification.
            (None, Some("notifications/tools/list_changed")) => {
                client.tools_stale.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
    // EOF: fail everything pending so callers see a clean error.
    if let Some(client) = client.upgrade() {
        client.pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}
