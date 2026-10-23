//! 0063 T4: under an allowlist that names the host, a URL server connects
//! with no ask. Its own binary because `net::init` is process-wide, set-once.

use std::sync::Arc;

use hotl_mcp::config::ServerConfig;
use hotl_mcp::trust::TrustStore;
use hotl_mcp::McpTool;
use hotl_tools::net::{self, EgressPolicy};
use hotl_tools::Tool;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

struct NoSecrets;
impl hotl_platform::SecretStore for NoSecrets {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}

/// A loopback MCP server speaking Streamable HTTP: JSON replies, one
/// request per connection is enough for reqwest's pool.
async fn mcp_origin() -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    let head_end = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i;
                        }
                        let mut tmp = [0u8; 4096];
                        match s.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + 4 + len {
                        let mut tmp = [0u8; 4096];
                        match s.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let body: Value =
                        serde_json::from_slice(&buf[head_end + 4..head_end + 4 + len])
                            .unwrap_or(Value::Null);
                    buf.drain(..head_end + 4 + len);
                    let id = body.get("id").cloned().unwrap_or(Value::Null);
                    let reply = match body.get("method").and_then(Value::as_str) {
                        Some("initialize") => Some(
                            json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18"}}),
                        ),
                        Some("tools/list") => Some(json!({"jsonrpc":"2.0","id":id,"result":{
                            "tools":[{"name":"ping","description":"d","inputSchema":{"type":"object"}}]}})),
                        _ => None,
                    };
                    let out = match reply {
                        Some(r) => {
                            let b = r.to_string();
                            format!(
                                "HTTP/1.1 200 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
                                b.len()
                            )
                        }
                        None => "HTTP/1.1 202 X\r\nContent-Length: 0\r\n\r\n".to_string(),
                    };
                    if s.write_all(out.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}/mcp")
}

#[tokio::test]
async fn an_allowlisted_host_connects() {
    net::init(EgressPolicy::Allowlist(vec!["127.0.0.1".to_string()]));
    let dir = tempfile::tempdir().unwrap();
    let url = mcp_origin().await;
    let cfg = ServerConfig::http("local", &url);
    let tool =
        McpTool::new_with_secrets(vec![cfg], TrustStore::load(dir.path()), Arc::new(NoSecrets));
    let _ = tool.permission(&json!({"server": "local"}));
    let out = tool
        .run(json!({"server": "local"}), CancellationToken::new())
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("ping"), "{}", out.content);
}
