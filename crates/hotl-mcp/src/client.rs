//! Newline-delimited JSON-RPC 2.0 over stdio (the MCP stdio transport).
//!
//! One reader task per connection routes responses to pending requests by
//! id, flips the staleness flag on `tools/list_changed`, and answers any
//! server→client request with "method not found" so a chatty server can't
//! wedge the pipe. A dead server surfaces as errors on pending requests —
//! never a harness crash.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Resolves when the caller cancels; never, when there is no token to watch.
async fn wait_for_cancel(cancel: Option<&CancellationToken>) {
    match cancel {
        Some(c) => c.cancelled().await,
        None => std::future::pending().await,
    }
}

const REQUEST_TIMEOUT_SECS: u64 = 30;
/// `tools/call` runs real work (builds, migrations, browser automation) and
/// gets a far longer leash than the protocol chatter above.
const TOOL_CALL_TIMEOUT_SECS: u64 = 600;
/// A write that has not completed in this long means the server has stopped
/// reading its stdin. The writer mutex is a single serialization point for the
/// whole server, so an unbounded write inside it hangs *every* request to that
/// server — including the `notifications/cancelled` that would have unblocked
/// the situation. The timeout covers lock acquisition too.
#[cfg(not(test))]
const WRITE_TIMEOUT_SECS: u64 = 20;
/// Same timeout path in tests, where the stuck peer is deliberate: 2s is
/// ample to prove the write times out, and 20s of sleep was most of the
/// suite's wall clock.
#[cfg(test)]
const WRITE_TIMEOUT_SECS: u64 = 2;
/// A single JSON-RPC message. Generous (`tools/list` with large schemas is
/// real) but finite: an endless line without `\n` used to grow a String until
/// OOM, and the 50 KiB sanitizer cap applies only after the message is in
/// memory.
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;
/// A dying server writes its diagnostics and exits; stdout EOF and the stderr
/// drain race. One short, bounded wait so the server's own last words make it
/// into the error we hand back.
const STDERR_GRACE_MS: u64 = 100;
const STDERR_RING_LINES: usize = 32;
const STDERR_RING_BYTES: usize = 8 * 1024;
pub const PROTOCOL_VERSION: &str = "2025-06-18";

type Pending = Mutex<HashMap<u64, oneshot::Sender<Value>>>;
type Writer = tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>;

/// JSON-RPC 2.0 ids may be a number *or* a string, and many servers echo the
/// type they received. We always *send* integers; we must accept and echo back
/// whatever a server uses (T2-13d).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Id {
    Num(u64),
    Str(String),
}

impl Id {
    fn from_json(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_u64().map(Id::Num),
            // A string id we sent as a number: match it back to the number.
            Value::String(s) => Some(
                s.parse::<u64>()
                    .map(Id::Num)
                    .unwrap_or_else(|_| Id::Str(s.clone())),
            ),
            _ => None,
        }
    }
    fn to_json(&self) -> Value {
        match self {
            Id::Num(n) => json!(n),
            Id::Str(s) => json!(s),
        }
    }
}

/// The tail of a server's stderr, bounded in both lines and bytes.
#[derive(Default)]
struct StderrRing {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrRing {
    fn push(&mut self, line: String) {
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.lines.len() > STDERR_RING_LINES || self.bytes > STDERR_RING_BYTES {
            match self.lines.pop_front() {
                Some(dropped) => self.bytes -= dropped.len(),
                None => break,
            }
        }
    }
}

pub struct Client {
    next_id: AtomicU64,
    pending: Pending,
    writer: Writer,
    /// Set by `notifications/tools/list_changed`; the tool re-lists on next use.
    pub tools_stale: AtomicBool,
    /// Once set, the connection is unusable: EOF, read error, oversized
    /// message, or write timeout. Callers check it instead of waiting out a
    /// timeout for a reply that can never arrive.
    dead: AtomicBool,
    last_error: Mutex<Option<String>>,
    stderr: Mutex<StderrRing>,
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
            // T2-13e: a crashing server's own error output is the most useful
            // diagnostic there is; it used to go to /dev/null.
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not start `{command}`: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take();
        let client = Self::start(stdout, stdin, Some(child));
        if let Some(stderr) = stderr {
            // Same `Weak` discipline as the reader task, so the drain cannot
            // pin the child alive after the last user drops the client.
            tokio::spawn(stderr_task(stderr, Arc::downgrade(&client)));
        }
        Ok(client)
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
            dead: AtomicBool::new(false),
            last_error: Mutex::new(None),
            stderr: Mutex::new(StderrRing::default()),
            _child: child,
        });
        // The reader holds only a Weak: when the last user drops the client,
        // the child's stdin drops with it (a server that exits on stdin EOF
        // sees it) and the reader task winds down instead of pinning both.
        tokio::spawn(reader_task(read, Arc::downgrade(&client)));
        client
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// The reason this connection died, with the server's own stderr tail
    /// appended. Composed at read time so a late-arriving diagnostic still
    /// lands in the message the model sees.
    pub fn last_error(&self) -> Option<String> {
        let reason = self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        let tail = self.stderr_tail();
        Some(if tail.is_empty() {
            reason
        } else {
            format!("{reason}\nserver stderr:\n{tail}")
        })
    }

    /// The read side `tools_stale` never had: takes the flag, so a change is
    /// announced once rather than on every later call.
    pub fn tools_changed(&self) -> bool {
        self.tools_stale.swap(false, Ordering::Relaxed)
    }

    fn stderr_tail(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lines
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Mark the connection unusable and fail everything waiting on it. Called
    /// on EOF, read error, oversized message, and write timeout.
    ///
    /// INVARIANT: after `die`, no caller waits on a timeout for a reply that
    /// can never arrive. Enforced by `an_unbounded_line_is_bounded`,
    /// `a_blocked_writer_does_not_hang_forever`, and
    /// `eof_fails_pending_immediately`.
    fn die(&self, reason: String) {
        self.dead.store(true, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(reason);
        // Dropping the senders wakes every pending receiver with a RecvError,
        // which `request_with_timeout` already maps to "server disconnected".
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// The error a dead connection reports, after one bounded chance for the
    /// stderr drain to catch up.
    async fn death_note(&self, fallback: &str) -> String {
        if self.stderr_tail().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(STDERR_GRACE_MS)).await;
        }
        self.last_error().unwrap_or_else(|| fallback.to_string())
    }

    async fn send(&self, msg: &Value) -> Result<(), String> {
        let mut line = msg.to_string();
        line.push('\n');
        // The timeout covers lock acquisition too — timing out only the inner
        // write would leave callers queued behind a stuck holder (T2-13b).
        let write = async {
            let mut w = self.writer.lock().await;
            w.write_all(line.as_bytes())
                .await
                .map_err(|e| format!("server pipe closed: {e}"))?;
            w.flush()
                .await
                .map_err(|e| format!("server pipe closed: {e}"))
        };
        match tokio::time::timeout(std::time::Duration::from_secs(WRITE_TIMEOUT_SECS), write).await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                // A write failure is the EPIPE flavor of a server crash: the
                // process died before this request reached it. Its stderr is
                // the diagnostic that matters, so compose the error the same
                // way the EOF path does — with the tail, after its grace.
                self.die(e.clone());
                Err(self.death_note(&e).await)
            }
            Err(_) => {
                // The write may have partially landed, so the stream is
                // desynced: the connection is not reusable.
                let e = format!(
                    "the server stopped reading its stdin (write timed out after \
                     {WRITE_TIMEOUT_SECS}s); the connection is now closed. Retry — \
                     hotl will start a fresh server process."
                );
                self.die(e.clone());
                Err(e)
            }
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT_SECS, None)
            .await
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout_secs: u64,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, String> {
        // T2-13a: a request is never issued to a connection already known dead
        // — that is what turned a crashed server into a full 600s wait.
        if self.is_dead() {
            return Err(self.death_note("server disconnected").await);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        // Every early return (send failure, timeout) must remove the entry
        // again or it leaks forever; the guard makes that unskippable.
        let guard = PendingGuard {
            pending: &self.pending,
            id: Some(id),
        };
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        // `biased` so an already-cancelled token wins deterministically rather
        // than racing the timeout branch. `PendingGuard` removes the pending
        // entry when the dropped branch's future dies — which is exactly what
        // makes the select-drop safe.
        let reply = tokio::select! {
            biased;
            _ = wait_for_cancel(cancel) => {
                // Same reasoning as the timeout arm: without this the server
                // keeps computing work nobody is waiting for, and a user
                // pressing ESC repeatedly accumulates orphan server-side work.
                self.cancel_server_side(id, "the user cancelled this call").await;
                return Err("the user cancelled this call".to_string());
            }
            r = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx) => match r {
                Ok(Ok(reply)) => reply,
                // The sender was dropped: the reader died and cleared pending.
                Ok(Err(_)) => return Err(self.death_note("server disconnected").await),
                Err(_) => {
                    self.cancel_server_side(id, &format!("timed out after {timeout_secs}s"))
                        .await;
                    return Err(format!("`{method}` timed out after {timeout_secs}s"));
                }
            },
        };
        // A reply means the reader already removed the entry.
        guard.disarm();
        if let Some(err) = reply.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("server error on `{method}`: {msg}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Tell the server to stop the work — without this a timed-out or
    /// cancelled call keeps running server-side while the model, told it
    /// failed, retries it into a duplicate.
    async fn cancel_server_side(&self, id: u64, reason: &str) {
        let _ = self
            .send(&json!({
                "jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": id, "reason": reason},
            }))
            .await;
    }

    pub async fn notify(&self, method: &str) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method}))
            .await
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
        self.call_tool_inner(name, arguments, None).await
    }

    /// `call_tool` that honours the user's cancellation token (T2-13c): ESC
    /// used to wait out the full 600s `tools/call` leash.
    pub async fn call_tool_cancellable(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<(String, bool), String> {
        self.call_tool_inner(name, arguments, Some(cancel)).await
    }

    async fn call_tool_inner(
        &self,
        name: &str,
        arguments: Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<(String, bool), String> {
        let result = self
            .request_with_timeout(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
                TOOL_CALL_TIMEOUT_SECS,
                cancel,
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
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
    v.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
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
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
        }
    }
}

/// Drains the child's stderr into the ring so a crash reports the server's own
/// diagnostics (T2-13e). Holds a `Weak`, like the reader.
async fn stderr_task(read: impl AsyncRead + Send + Unpin, client: Weak<Client>) {
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Some(client) = client.upgrade() else {
            return;
        };
        // Server-controlled bytes: stripped before they can reach an error
        // string, and enveloped again at the tool boundary.
        let line = crate::sanitize::safe_summary(&line);
        if line.is_empty() {
            continue;
        }
        client
            .stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }
}

async fn reader_task(read: impl AsyncRead + Send + Unpin, client: Weak<Client>) {
    let mut reader = BufReader::new(read);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // `take` is recreated each iteration so the limit is per-message.
        let mut limited = (&mut reader).take(MAX_LINE_BYTES);
        match limited.read_until(b'\n', &mut buf).await {
            Ok(0) => break, // EOF
            Ok(_) if buf.last() != Some(&b'\n') => {
                // Hit the cap without a terminator: the stream is desynced and
                // there is no way to resynchronize.
                if let Some(client) = client.upgrade() {
                    client.die(format!(
                        "the server sent a message that is too large (over \
                         {MAX_LINE_BYTES} bytes with no newline terminator); the \
                         connection is now closed. Retry — hotl will start a fresh \
                         server process."
                    ));
                }
                return;
            }
            Ok(_) => {}
            Err(e) => {
                if let Some(client) = client.upgrade() {
                    client.die(format!("read error from the server: {e}"));
                }
                return;
            }
        }
        // Upgrade per message: a Weak here means the last user is gone and
        // the task must not keep the client (and the child's stdin) alive.
        let Some(client) = client.upgrade() else {
            return;
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        let id = msg
            .get("id")
            .filter(|v| !v.is_null())
            .and_then(Id::from_json);
        let method = msg.get("method").and_then(Value::as_str);
        match (id, method) {
            // Response to one of our requests. We only ever issue numeric ids,
            // so a string id that parses as one routes back to it (T2-13d).
            (Some(Id::Num(id)), None) => {
                if let Some(tx) = client
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id)
                {
                    let _ = tx.send(msg);
                }
            }
            // A response carrying an id we never issued: nothing to route to.
            (Some(Id::Str(_)), None) => {}
            // Server→client request: refuse politely so nothing hangs, echoing
            // the id back in the JSON type it arrived in.
            (Some(id), Some(_)) => {
                let _ = client
                    .send(&json!({
                        "jsonrpc": "2.0", "id": id.to_json(),
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
        client.die("the server disconnected (it closed its stdout or exited)".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    /// Reads one line from a raw half without a `BufReader` wrapper, so the
    /// caller keeps ownership of the stream for the rest of the exchange.
    async fn next_line(read: &mut (impl AsyncRead + Unpin), buf: &mut Vec<u8>) -> Option<String> {
        loop {
            if let Some(i) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8(buf[..i].to_vec()).ok()?;
                buf.drain(..=i);
                return Some(line);
            }
            let mut chunk = [0u8; 4096];
            match read.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    #[tokio::test]
    async fn a_string_id_response_completes_the_request() {
        // T2-13d: `"id": "1"` used to match no arm; the caller waited 30s.
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server_end);
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let msg: Value = serde_json::from_str(&line).unwrap();
                let id = msg.get("id").and_then(Value::as_u64).unwrap();
                // Echo the id back as a *string* — legal JSON-RPC 2.0.
                let out = json!({"jsonrpc":"2.0","id":id.to_string(),"result":{"ok":true}});
                write
                    .write_all(format!("{out}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let (r, w) = tokio::io::split(client_end);
        let client = Client::from_streams(r, w);
        let got = tokio::time::timeout(Duration::from_secs(2), client.request("ping", json!({})))
            .await
            .expect("must not wait out the 30s timeout")
            .expect("ok");
        assert_eq!(got, json!({"ok": true}));
    }

    #[tokio::test]
    async fn a_server_request_with_a_string_id_is_answered() {
        // T2-13d second half: the -32601 wedge-breaker must fire for string
        // ids too, echoing the id back in the same JSON type it arrived in.
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (r, w) = tokio::io::split(client_end);
        let _client = Client::from_streams(r, w);
        let (mut sread, mut swrite) = tokio::io::split(server_end);
        swrite
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":\"abc\",\"method\":\"sampling/createMessage\"}\n",
            )
            .await
            .unwrap();
        let mut buf = Vec::new();
        let line = tokio::time::timeout(Duration::from_secs(5), next_line(&mut sread, &mut buf))
            .await
            .expect("a server request must be answered, not ignored")
            .expect("a reply line");
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            reply.get("id"),
            Some(&json!("abc")),
            "same JSON type: {line}"
        );
        assert_eq!(reply.pointer("/error/code"), Some(&json!(-32601)));
    }

    #[tokio::test]
    async fn an_unbounded_line_is_bounded() {
        // Endless bytes, never a newline: must fail fast, not grow until OOM.
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let (_r, mut write) = tokio::io::split(server_end);
            let chunk = vec![b'x'; 64 * 1024];
            while write.write_all(&chunk).await.is_ok() {}
        });
        let (r, w) = tokio::io::split(client_end);
        let client = Client::from_streams(r, w);
        let err = tokio::time::timeout(Duration::from_secs(10), client.request("ping", json!({})))
            .await
            .expect("must not hang")
            .expect_err("oversized line is a protocol error");
        assert!(
            err.contains("too large") || err.contains("disconnected"),
            "{err}"
        );
        assert!(client.is_dead(), "a desynced stream is dead, not reusable");
    }

    // Real time, not `start_paused`: pausing needs tokio's `test-util` feature,
    // and this plan adds no workspace dependency changes. The cost is one test
    // that waits out `WRITE_TIMEOUT_SECS`.
    #[tokio::test]
    async fn a_blocked_writer_does_not_hang_forever() {
        // T2-13b: the server never reads stdin; the pipe fills; `send` used to
        // block inside the writer mutex, wedging every other request.
        let (client_end, server_end) = tokio::io::duplex(1024); // tiny pipe
                                                                // Held, never read, never closed — the pipe must stay full.
        let _kept = server_end;
        let (r, w) = tokio::io::split(client_end);
        let client = Client::from_streams(r, w);
        let big = json!({"blob": "x".repeat(1024 * 1024)});
        let err = tokio::time::timeout(Duration::from_secs(60), client.request("ping", big))
            .await
            .expect("must not hang past the write timeout")
            .expect_err("write must time out");
        assert!(
            err.contains("stopped reading") || err.contains("timed out"),
            "{err}"
        );
        assert!(client.is_dead());
    }

    #[tokio::test]
    async fn eof_fails_pending_immediately() {
        // A request already in flight when the server dies must not wait out
        // the timeout for a reply that can never arrive.
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let (read, _write) = tokio::io::split(server_end);
            let mut lines = BufReader::new(read).lines();
            let _ = lines.next_line().await; // take the request, then hang up
        });
        let (r, w) = tokio::io::split(client_end);
        let client = Client::from_streams(r, w);
        let err = tokio::time::timeout(Duration::from_secs(5), client.request("ping", json!({})))
            .await
            .expect("must not wait out the 30s timeout")
            .expect_err("the server hung up");
        assert!(err.contains("disconnected"), "{err}");
        assert!(client.is_dead());
    }

    // Spawns a real `/bin/sh`; the portable stderr-tail path (EPIPE on write)
    // is covered by `a_failed_write_still_carries_the_stderr_tail` below.
    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_tail_reaches_the_disconnect_error() {
        // T2-13e: a crashing server's own diagnostics used to go to /dev/null.
        let client = Client::connect(
            "/bin/sh",
            &[
                "-c".into(),
                "echo 'FATAL: config.json not found' >&2; exit 1".into(),
            ],
        )
        .unwrap();
        let err = client
            .initialize()
            .await
            .expect_err("the server exits immediately");
        assert!(
            err.contains("config.json not found"),
            "stderr tail must be included: {err}"
        );
    }

    #[tokio::test]
    async fn a_failed_write_still_carries_the_stderr_tail() {
        // The other way a crashed server surfaces: its stdin is already gone
        // when the request is written, so the failure is EPIPE on the write,
        // not EOF on the read. The tail must reach that error too — on a slow
        // machine the test above lands on this path, which is what failed the
        // v0.5.2 and v0.7.0 release gates.
        let (reader, _held_open) = tokio::io::duplex(64);
        let (writer, closed) = tokio::io::duplex(64);
        drop(closed); // every write now fails with BrokenPipe
        let client = Client::from_streams(reader, writer);
        client
            .stderr
            .lock()
            .unwrap()
            .push("FATAL: config.json not found".into());
        let err = client
            .request("ping", json!({}))
            .await
            .expect_err("the write must fail");
        assert!(
            err.contains("config.json not found"),
            "stderr tail must be included: {err}"
        );
        assert!(client.is_dead());
    }
}
