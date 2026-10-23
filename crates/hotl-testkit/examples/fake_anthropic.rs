//! Hand-run the scripted Anthropic SSE server (0074 T4):
//!
//! ```sh
//! cargo run -p hotl-testkit --example fake_anthropic -- script.jsonl
//! # prints http://127.0.0.1:PORT, then serves until ctrl-c
//! ANTHROPIC_API_KEY=x HOTL_ANTHROPIC_BASE_URL=http://127.0.0.1:PORT hotl -p "go"
//! ```
//!
//! `script.jsonl` is one reply per line, a direct mapping onto the fake's
//! builders — no second format:
//!
//! ```text
//! {"text": "hello"}
//! {"thinking": "hmm", "text": "then this"}
//! {"tool_use": {"id": "t1", "name": "bash", "input": {"command": "ls"}}}
//! {"tool_use": {"id": "t2", "name": "write", "input": {"path": "big.txt", "content": "…"}}, "chunk_bytes": 1024, "per_chunk_ms": 1000}
//! {"route": "child-A", "text": "the child's answer", "after_ms": 3000}
//! {"text": "priced", "usage": {"input": 20, "output": 9, "cache_read": 300, "cache_5m": 150, "cache_1h": 300}}
//! {"http_error": 529, "kind": "overloaded_error"}
//! ```
//!
//! A line with `route` is tried first against any request whose body
//! contains it (consumed on first match); every other line joins the queue
//! in order; when both run out, a short text reply.

use std::time::Duration;

use hotl_testkit::fake_anthropic::{self as fake, FakeAnthropic, Message, Reply, Script, Usage};
use serde_json::Value;

fn message(line: &Value) -> Option<Message> {
    let mut parts: Vec<Message> = Vec::new();
    if let Some(t) = line.get("thinking").and_then(Value::as_str) {
        parts.push(fake::thinking(t));
    }
    if let Some(t) = line.get("text").and_then(Value::as_str) {
        parts.push(fake::text(t));
    }
    if let Some(tu) = line.get("tool_use") {
        let id = tu.get("id").and_then(Value::as_str).unwrap_or("t1");
        let name = tu.get("name").and_then(Value::as_str).unwrap_or("bash");
        let input = tu
            .get("input")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let m = match line.get("chunk_bytes").and_then(Value::as_u64) {
            Some(bytes) => fake::tool_use_streamed(
                id,
                name,
                input,
                bytes as usize,
                Duration::from_millis(
                    line.get("per_chunk_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                ),
            ),
            None => fake::tool_use(id, name, input),
        };
        parts.push(m);
    }
    if parts.is_empty() {
        return None;
    }
    let mut m = fake::batch(parts);
    if let Some(u) = line.get("usage") {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        m = m.with_usage(Usage {
            input: n("input"),
            output: n("output"),
            cache_read: n("cache_read"),
            cache_5m: n("cache_5m"),
            cache_1h: n("cache_1h"),
        });
    }
    if let Some(ms) = line.get("after_ms").and_then(Value::as_u64) {
        m = m.after(Duration::from_millis(ms));
    }
    if let Some(ms) = line.get("pace_ms").and_then(Value::as_u64) {
        m = m.paced(Duration::from_millis(ms));
    }
    Some(m)
}

fn reply(line: &Value) -> Option<Reply> {
    if let Some(status) = line.get("http_error").and_then(Value::as_u64) {
        let kind = line
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("api_error");
        return Some(fake::http_error(status as u16, kind));
    }
    message(line).map(Reply::from)
}

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: fake_anthropic <script.jsonl>");
        std::process::exit(2);
    });
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(2);
    });
    let mut script = Script::default();
    for (n, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let line: Value = serde_json::from_str(raw).unwrap_or_else(|e| {
            eprintln!("{path}:{}: {e}", n + 1);
            std::process::exit(2);
        });
        let Some(r) = reply(&line) else {
            eprintln!("{path}:{}: no reply in this line", n + 1);
            std::process::exit(2);
        };
        match line.get("route").and_then(Value::as_str) {
            Some(needle) => script = script.route(needle, r),
            None => script = script.then(r),
        }
    }
    let fake = FakeAnthropic::start(script).await;
    println!("{}", fake.base_url());
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("{} request(s) served", fake.request_count());
}
