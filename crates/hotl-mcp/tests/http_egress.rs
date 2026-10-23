//! 0063 T4: a URL server takes the same egress check `web_fetch` does, before
//! any socket. Its own binary because `net::init` is process-wide, set-once.

use std::sync::Arc;

use hotl_mcp::config::ServerConfig;
use hotl_mcp::trust::TrustStore;
use hotl_mcp::McpTool;
use hotl_tools::net::{self, EgressPolicy};
use hotl_tools::Tool;
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct NoSecrets;
impl hotl_platform::SecretStore for NoSecrets {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}

#[tokio::test]
async fn egress_off_refuses_a_url_server_with_the_policy_reason() {
    net::init(EgressPolicy::Off);
    let dir = tempfile::tempdir().unwrap();
    // Nothing listens here; the refusal must come from the policy, not a
    // connection failure.
    let cfg = ServerConfig::http("gh", "http://127.0.0.1:9/mcp");
    let tool =
        McpTool::new_with_secrets(vec![cfg], TrustStore::load(dir.path()), Arc::new(NoSecrets));
    let _ = tool.permission(&json!({"server": "gh"}));
    let out = tool
        .run(json!({"server": "gh"}), CancellationToken::new())
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("[network] egress") && out.content.contains("not connected"),
        "{}",
        out.content
    );
}
