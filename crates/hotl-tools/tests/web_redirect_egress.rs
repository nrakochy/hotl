//! Finding 1 (Plan 5 review): a bare `.timeout()`/`.user_agent()` client
//! uses reqwest's DEFAULT redirect policy — follow up to 10 hops with no
//! re-check. Under `[network] egress = "allowlist"`, the pre-flight
//! `net::host_allowed` check in `web_fetch`/`web_search` covers only the
//! *first* hop's host; an allowed origin that 302-redirects to a host never
//! in `allow` would have its response silently followed and returned,
//! bypassing the allowlist entirely — the redirect target is never asked
//! about. `WebFetchTool`/`WebSearchTool` now install a custom
//! `redirect::Policy` that re-checks `net::host_allowed` on every hop, so a
//! redirect to a denied host fails closed exactly like a first-hop denial
//! would.
//!
//! This lives in its own integration-test binary (a separate process) for
//! the same reason `web_egress.rs` does: `net::init` installs the
//! process-wide, set-once `EgressPolicy` — calling it from the `hotl-tools`
//! unit-test binary, or adding a second `net::init` call to `web_egress.rs`,
//! would race with that file's own `Off` policy inside one shared process.

use hotl_tools::concurrency::{ConcurrencyLimits, SessionConcurrency};
use hotl_tools::net::{self, EgressPolicy};
use hotl_tools::web::WebFetchTool;
use hotl_tools::Tool;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

fn test_concurrency() -> SessionConcurrency {
    SessionConcurrency::new(ConcurrencyLimits::default())
}

/// A loopback origin that answers every connection with a fixed HTTP
/// response (a full status line + headers + body, verbatim).
async fn respond_with(response: String) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut s, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(response.as_bytes()).await;
        }
    });
    port
}

#[tokio::test]
async fn redirect_to_a_denied_host_fails_closed_and_never_returns_its_body() {
    // Only 127.0.0.1 is allowed — the redirect target below (a distinct,
    // unresolvable hostname) is not, and must never be reached.
    net::init(EgressPolicy::Allowlist(vec!["127.0.0.1".to_string()]));

    let origin_port = respond_with(
        "HTTP/1.1 302 Found\r\nlocation: http://evil.invalid.example/payload\r\ncontent-length: 0\r\n\r\n"
            .to_string(),
    )
    .await;

    let tool = WebFetchTool::new(test_concurrency());
    let url = format!("http://127.0.0.1:{origin_port}/");
    let out = tool
        .run(json!({"urls": [url]}), CancellationToken::new())
        .await;

    assert!(
        out.is_error,
        "a redirect to a host outside [network].allow must fail closed: {}",
        out.content
    );
    assert!(
        out.content.contains("evil.invalid.example"),
        "the refusal must name the denied redirect host: {}",
        out.content
    );
    assert!(
        !out.content.contains("payload"),
        "the denied host must never even be dialed, let alone its body surfaced: {}",
        out.content
    );
}

#[tokio::test]
async fn redirect_between_two_allowed_hosts_still_succeeds() {
    net::init(EgressPolicy::Allowlist(vec!["127.0.0.1".to_string()]));

    let target_body = "redirected-ok";
    let target_port = respond_with(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{}",
        target_body.len(),
        target_body
    ))
    .await;
    let origin_port = respond_with(format!(
        "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{target_port}/\r\ncontent-length: 0\r\n\r\n"
    ))
    .await;

    let tool = WebFetchTool::new(test_concurrency());
    let url = format!("http://127.0.0.1:{origin_port}/");
    let out = tool
        .run(json!({"urls": [url]}), CancellationToken::new())
        .await;

    assert!(
        !out.is_error,
        "a redirect between two allowed hosts must still succeed: {}",
        out.content
    );
    assert!(out.content.contains(target_body), "{}", out.content);
}
