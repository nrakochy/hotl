//! Task 4 (Phase 3): `compose_request` must carry `EngineConfig::cache_ttl`
//! faithfully into `CachePolicy::Static { prefix_ttl }` — the one thing this
//! layer does with the knob; the Anthropic serializer decides which markers
//! actually honor it.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::{CachePolicy, CacheTtl, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};

async fn run_one_turn_and_capture(config: EngineConfig) -> hotl_provider::SamplingRequest {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let mut handle = spawn_session(SessionDeps {
        provider: provider.clone(),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        config,
    });
    handle.prompt("go".into()).await;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if matches!(ev, EngineEvent::TurnDone { .. }) {
            break;
        }
    }
    provider.last_request().expect("one request")
}

/// The default config (`cache_ttl: FiveMinutes`) must still request
/// `Static { prefix_ttl: FiveMinutes }` — this phase changes nothing about
/// today's behavior unless a surface opts a session into `OneHour`.
#[tokio::test]
async fn default_config_composes_a_five_minute_static_policy() {
    let request = run_one_turn_and_capture(EngineConfig::default()).await;
    assert_eq!(
        request.cache,
        CachePolicy::Static {
            prefix_ttl: CacheTtl::FiveMinutes
        }
    );
}

/// `cfg.cache_ttl` is read, not just constructed: an `OneHour` config must
/// produce an `OneHour` policy on the wire request the provider actually saw.
#[tokio::test]
async fn one_hour_cache_ttl_composes_a_one_hour_static_policy() {
    let config = EngineConfig {
        cache_ttl: CacheTtl::OneHour,
        ..EngineConfig::default()
    };
    let request = run_one_turn_and_capture(config).await;
    assert_eq!(
        request.cache,
        CachePolicy::Static {
            prefix_ttl: CacheTtl::OneHour
        }
    );
}

/// `cache_static: false` must still map to `Off` regardless of `cache_ttl` —
/// the ttl is only ever consumed when explicit breakpoints are on at all.
#[tokio::test]
async fn cache_static_false_stays_off_regardless_of_ttl() {
    let config = EngineConfig {
        cache_static: false,
        cache_ttl: CacheTtl::OneHour,
        ..EngineConfig::default()
    };
    let request = run_one_turn_and_capture(config).await;
    assert_eq!(request.cache, CachePolicy::Off);
}
