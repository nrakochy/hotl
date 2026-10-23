//! 0073 T2 — the cache keep-alive around a tool batch. A long `bash` on the
//! 5-minute TTL gets a `max_tokens: 1` re-send of the last request; the
//! 1-hour TTL, an unpriced model, a small prefix, a fast tool and the
//! `cache_warming` switch all get none; the ping's bill reaches the turn's
//! usage, the cost meter and the log; a 400 latches for the session.
//!
//! Real tokio time: the actor's log writer is involved. The deadline is the
//! `cache_keepalive_after` test seam; the scripted sample reports a 100k
//! cache read so the projected saving clears Pi's $0.05 rule on Opus 5.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{CacheTtl, ProviderError, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{StopReason, TokenUsage};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    log_path: std::path::PathBuf,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<ScriptedProvider>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        initial_decisions: Vec::new(),
        plan_files: None,
        config,
    });
    Session {
        handle,
        log_path,
        dir,
    }
}

fn config(after_ms: u64) -> EngineConfig {
    EngineConfig {
        model: "claude-opus-5".into(),
        cache_static: true,
        cache_ttl: CacheTtl::FiveMinutes,
        cache_keepalive_after: Duration::from_millis(after_ms),
        ..EngineConfig::default()
    }
}

/// A sample that calls `bash sleep <secs>` and reports `read` cached tokens.
fn long_bash(secs: &str, read: u64) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut script =
        ScriptedProvider::tool_call("t1", "bash", json!({"command": format!("sleep {secs}")}));
    if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
        usage.cache_read_input_tokens = read;
    }
    script
}

/// What a keep-alive ping is answered with: one output token, the prefix read.
fn one_token(read: u64) -> Vec<Result<StreamEvent, ProviderError>> {
    vec![
        Ok(StreamEvent::Started { request_id: None }),
        Ok(StreamEvent::Completed {
            stop: StopReason::MaxTokens,
            usage: TokenUsage {
                cache_read_input_tokens: read,
                output_tokens: 1,
                ..Default::default()
            },
            blocks: Vec::new(),
        }),
    ]
}

struct Ran {
    outcome: Outcome,
    usage: TokenUsage,
    warmed: Vec<(TokenUsage, Option<f64>)>,
    notices: Vec<(u8, f64, f64)>,
    /// Every `CacheWarmed` arrived before `TurnDone`.
    warmed_before_done: bool,
}

/// Drive one turn, allowing every ask.
async fn run(s: &mut Session) -> Ran {
    s.handle.prompt("go".into()).await;
    let mut warmed = Vec::new();
    let mut notices = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::CacheWarmed { usage, cost_usd } => warmed.push((usage, cost_usd)),
            EngineEvent::BudgetNotice {
                pct,
                used_usd,
                cap_usd,
            } => notices.push((pct, used_usd, cap_usd)),
            EngineEvent::TurnDone { outcome, usage, .. } => {
                return Ran {
                    outcome,
                    usage,
                    warmed,
                    notices,
                    warmed_before_done: true,
                }
            }
            _ => {}
        }
    }
}

fn kinds(log_path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log_path)
        .expect("log")
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            v.get("payload")
                .or(Some(&v))
                .and_then(|p| p.get("kind"))
                .and_then(|k| k.as_str())
                .map(str::to_string)
        })
        .collect()
}

#[tokio::test]
async fn a_long_bash_call_on_a_five_minute_ttl_gets_a_keep_alive_ping() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        long_bash("0.6", 100_000),
        ScriptedProvider::text_reply("done"),
    ]));
    provider.set_ping_reply(one_token(100_000));
    let mut s = session(provider.clone(), config(50));
    let ran = run(&mut s).await;
    assert!(
        matches!(ran.outcome, Outcome::Done { .. }),
        "{:?}",
        ran.outcome
    );

    let pings = provider.ping_count();
    assert!(
        pings >= 1,
        "no ping in {} requests",
        provider.request_count()
    );
    assert_eq!(provider.request_count(), 2 + pings);
    let reqs = provider.requests();
    let first = &reqs[0];
    let ping = reqs.iter().find(|r| r.max_tokens == 1).expect("a ping");
    // D1: the ping is the last request with one field changed.
    assert_eq!(format!("{:?}", ping.items), format!("{:?}", first.items));
    assert_eq!(ping.system, first.system);
    assert_eq!(ping.tools.len(), first.tools.len());
    assert_eq!(ping.cache, first.cache);
    assert_eq!(ping.turn_context, first.turn_context);

    // D6: labelled on the wire, folded into the turn's usage.
    assert_eq!(ran.warmed.len(), pings);
    assert!(ran.warmed_before_done);
    assert!(ran.warmed[0].1.is_some(), "Opus 5 is priced");
    assert_eq!(
        ran.usage.cache_read_input_tokens,
        100_000 * (1 + pings as u64),
        "the sample's read plus one per ping"
    );
    // The log: one `usage` entry per sample plus one per ping, no extra items.
    let kinds = kinds(&s.log_path);
    let items = kinds.iter().filter(|k| *k == "item").count();
    let usages = kinds.iter().filter(|k| *k == "usage").count();
    assert_eq!(
        items, 4,
        "user, assistant, tool results, assistant: {kinds:?}"
    );
    assert_eq!(usages, 2 + pings, "{kinds:?}");
}

#[tokio::test]
async fn a_one_hour_ttl_never_pings() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        long_bash("0.3", 100_000),
        ScriptedProvider::text_reply("done"),
    ]));
    provider.set_ping_reply(one_token(100_000));
    let mut s = session(
        provider.clone(),
        EngineConfig {
            cache_ttl: CacheTtl::OneHour,
            ..config(50)
        },
    );
    let ran = run(&mut s).await;
    assert!(matches!(ran.outcome, Outcome::Done { .. }));
    assert_eq!(provider.request_count(), 2);
    assert!(ran.warmed.is_empty());
}

#[tokio::test]
async fn an_uncatalogued_model_never_pings() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        long_bash("0.3", 100_000),
        ScriptedProvider::text_reply("done"),
    ]));
    provider.set_ping_reply(one_token(100_000));
    let mut s = session(
        provider.clone(),
        EngineConfig {
            model: "sol-fast".into(),
            ..config(50)
        },
    );
    let ran = run(&mut s).await;
    assert!(matches!(ran.outcome, Outcome::Done { .. }));
    assert_eq!(provider.request_count(), 2);
}

#[tokio::test]
async fn a_small_prefix_is_not_worth_a_ping() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        long_bash("0.3", 2_000),
        ScriptedProvider::text_reply("done"),
    ]));
    provider.set_ping_reply(one_token(2_000));
    let mut s = session(provider.clone(), config(50));
    let ran = run(&mut s).await;
    assert!(matches!(ran.outcome, Outcome::Done { .. }));
    assert_eq!(provider.request_count(), 2);
}

#[tokio::test]
async fn a_fast_tool_finishes_before_the_deadline() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        long_bash("0", 100_000),
        ScriptedProvider::text_reply("done"),
    ]));
    provider.set_ping_reply(one_token(100_000));
    let mut s = session(provider.clone(), config(2_000));
    let started = std::time::Instant::now();
    let ran = run(&mut s).await;
    assert!(matches!(ran.outcome, Outcome::Done { .. }));
    assert_eq!(provider.request_count(), 2);
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "the guard must abort the sleeping ping: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn cache_warming_false_never_pings() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        long_bash("0.3", 100_000),
        ScriptedProvider::text_reply("done"),
    ]));
    provider.set_ping_reply(one_token(100_000));
    let mut s = session(
        provider.clone(),
        EngineConfig {
            cache_warming: false,
            ..config(50)
        },
    );
    let ran = run(&mut s).await;
    assert!(matches!(ran.outcome, Outcome::Done { .. }));
    assert_eq!(provider.request_count(), 2);
}

/// Opus 5 reads at $0.50/Mtok: the sample's 100k read is ~$0.05, each ping's
/// 1M read is $0.50. With a $1.00 cap the sample alone is 5% and the first
/// ping crosses 50%. `max_tokens` is small so the pre-flight worst case
/// (a full output at list price) does not refuse the first sample.
#[tokio::test]
async fn a_ping_counts_against_the_cost_cap() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        long_bash("0.3", 100_000),
        ScriptedProvider::text_reply("done"),
    ]));
    provider.set_ping_reply(one_token(1_000_000));
    let mut s = session(
        provider.clone(),
        EngineConfig {
            max_cost_usd: 1.0,
            max_tokens: 1_000,
            ..config(50)
        },
    );
    let ran = run(&mut s).await;
    assert!(provider.ping_count() >= 1);
    let first = ran.notices.first().expect("a budget notice from the ping");
    assert_eq!(first.0, 50);
    assert!(
        first.1 >= 0.55,
        "the ping's $0.50 is in used_usd: {first:?}"
    );
    assert_eq!(first.2, 1.0);
}

#[tokio::test]
async fn a_rejected_ping_latches_for_the_session() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        long_bash("0.3", 100_000),
        ScriptedProvider::text_reply("done"),
        long_bash("0.3", 100_000),
        ScriptedProvider::text_reply("done again"),
    ]));
    provider.set_ping_reply(vec![Err(ProviderError::Http {
        status: 400,
        message: "max_tokens: 1 is below the minimum".into(),
        retry_after: None,
        request_id: None,
    })]);
    let mut s = session(provider.clone(), config(50));
    let first = run(&mut s).await;
    assert!(
        matches!(first.outcome, Outcome::Done { .. }),
        "{:?}",
        first.outcome
    );
    assert_eq!(provider.ping_count(), 1, "one probe, then the latch");
    assert!(first.warmed.is_empty(), "a refused ping bills nothing");
    let second = run(&mut s).await;
    assert!(matches!(second.outcome, Outcome::Done { .. }));
    assert_eq!(
        provider.ping_count(),
        1,
        "latched: the second batch never pings"
    );
    assert_eq!(provider.request_count(), 5);
}
