//! End-to-end egress fail-closed check for `web_fetch`/`web_search`, run as
//! its own process (a separate integration-test binary): `net::init`
//! installs the process-wide, set-once `EgressPolicy` — calling it from the
//! `hotl-tools` unit-test binary would leak into every other test sharing
//! that process (they all assume the untouched default, `Open`). This file
//! is its own binary, so installing `Off` here affects nothing else.
//!
//! Every other egress branch (`Allowed`/`NoPolicy`/the `Allowlist` match
//! itself) is covered by the pure `net::verdict_for` unit tests in
//! `net.rs`, which take the policy as a parameter and need no global state.

use hotl_tools::concurrency::{ConcurrencyLimits, SessionConcurrency};
use hotl_tools::net::{self, EgressPolicy};
use hotl_tools::web::{SearchBackend, WebFetchTool, WebSearchTool};
use hotl_tools::Tool;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn egress_off_refuses_web_fetch_and_web_search_without_a_request() {
    net::init(EgressPolicy::Off);

    let fetch = WebFetchTool::new(SessionConcurrency::new(ConcurrencyLimits::default()));
    let out = fetch
        .run(
            json!({"urls": ["https://example.com/"]}),
            CancellationToken::new(),
        )
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("egress is off"),
        "the refusal must name the reason, not just fail generically: {}",
        out.content
    );

    let search = WebSearchTool::new(
        SearchBackend {
            url: "https://search.example/api".into(),
            api_key: None,
            result_cap: 8,
        },
        SessionConcurrency::new(ConcurrencyLimits::default()),
    );
    let out = search
        .run(json!({"query": "rust"}), CancellationToken::new())
        .await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("egress is off"),
        "web_search must also fail closed: {}",
        out.content
    );
}
