//! Cache keep-alive (0073 half A): while a tool batch outlives the 5-minute
//! prompt cache, re-send the last sample's request with `max_tokens: 1`. A
//! cache read refreshes the entry's timer at read price, so the sample after
//! the tool reads the history at 0.1× instead of rewriting it at 1.25×.
//!
//! The request is the one the provider already saw, so every `cache_control`
//! marker lands on the same block; nothing here builds a request. The ping
//! fires only while a tool is running (the agent is provably about to sample
//! again), only on the 5-minute TTL, and only when the projected saving pays
//! for the ping (Pi's $0.05 rule).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use hotl_provider::{CachePolicy, CacheTtl, Provider, ProviderError, SamplingRequest, StreamEvent};
use hotl_types::TokenUsage;
use tokio::sync::mpsc;

/// Anthropic's default breakpoint lifetime, counted from the start of the
/// request that last touched the entry.
pub(crate) const CACHE_TTL_5M: Duration = Duration::from_secs(300);
/// Request latency plus time to first byte: the ping must land before the
/// entry is gone, not be sent before.
pub(crate) const KEEPALIVE_MARGIN: Duration = Duration::from_secs(45);
/// A ping fires only when the next sample would otherwise pay at least this
/// much more (Pi 0.86's rule). With Opus 5 rates that is a prefix of about
/// 9.5k tokens; Sonnet 5 about 24k; Fable 5 about 4.8k.
pub(crate) const KEEPALIVE_MIN_SAVING_USD: f64 = 0.05;

/// What the next sample would rewrite if the cache went cold: everything the
/// last sample read from or wrote to the cache.
pub(crate) fn prefix_tokens(usage: &TokenUsage) -> u64 {
    usage.cache_read_input_tokens + usage.cache_creation_input_tokens
}

/// `prefix × (write − read) − (prefix × read + one output token)`, in USD.
/// `None` for an uncatalogued model: no price, no ping (the `max_cost_usd`
/// rule, fail closed).
pub(crate) fn projected_saving_usd(model: &str, prefix: u64) -> Option<f64> {
    let m = hotl_provider::catalog::lookup(model)?;
    let per = |tokens: u64, rate: f64| (tokens as f64 / 1_000_000.0) * rate;
    let miss = per(prefix, m.cache_write_usd_per_mtok) - per(prefix, m.cache_read_usd_per_mtok);
    Some(miss - ping_cost_usd(model, prefix).unwrap_or(0.0))
}

/// What one ping bills: the prefix at read price plus one output token.
pub(crate) fn ping_cost_usd(model: &str, prefix: u64) -> Option<f64> {
    let m = hotl_provider::catalog::lookup(model)?;
    let per = |tokens: u64, rate: f64| (tokens as f64 / 1_000_000.0) * rate;
    Some(per(prefix, m.cache_read_usd_per_mtok) + per(1, m.output_usd_per_mtok))
}

/// The ping: the last sample's request with `max_tokens: 1` and nothing else
/// touched (D1) — the invariant the whole mechanism rests on.
pub(crate) fn ping_request(last: &SamplingRequest) -> SamplingRequest {
    SamplingRequest {
        max_tokens: 1,
        ..last.clone()
    }
}

/// The whole gate in one pure function: the 5-minute TTL only, the switch on,
/// the session not latched, the saving worth it, and the ping under the cost
/// cap (`cap_usd <= 0` is no cap).
pub(crate) fn should_ping(
    cache: CachePolicy,
    enabled: bool,
    rejected: bool,
    saving: Option<f64>,
    spent_usd: f64,
    cap_usd: f64,
    ping_cost: f64,
) -> bool {
    if !matches!(
        cache,
        CachePolicy::Static {
            prefix_ttl: CacheTtl::FiveMinutes
        }
    ) {
        return false;
    }
    if !enabled || rejected {
        return false;
    }
    let Some(saving) = saving else {
        return false;
    };
    if saving < KEEPALIVE_MIN_SAVING_USD {
        return false;
    }
    cap_usd <= 0.0 || spent_usd + ping_cost <= cap_usd
}

/// Aborts the ping task on drop (the `ArmGuard` shape): a batch that finished
/// before the deadline costs nothing.
#[must_use]
pub(crate) struct KeepAliveGuard {
    task: tokio::task::JoinHandle<()>,
    report: mpsc::UnboundedReceiver<TokenUsage>,
}

impl KeepAliveGuard {
    /// Stop pinging and collect what the pings so far were billed.
    pub(crate) fn drain(mut self) -> Vec<TokenUsage> {
        self.task.abort();
        let mut out = Vec::new();
        while let Ok(u) = self.report.try_recv() {
            out.push(u);
        }
        out
    }
}

impl Drop for KeepAliveGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) struct KeepAlive;

impl KeepAlive {
    /// Arm the task: sleep until `first_deadline`, ping, then re-arm `after`
    /// from each ping's own start for as long as the guard lives. A ping that
    /// sees `Retrying` or an error gives up on this round (D7) — the ladder's
    /// backoff would spend the very seconds the ping exists to beat — and a
    /// 400/422 latches `rejected` so the session pings once, not forever.
    pub(crate) fn arm(
        provider: Arc<dyn Provider>,
        request: SamplingRequest,
        first_deadline: tokio::time::Instant,
        after: Duration,
        rejected: Arc<AtomicBool>,
    ) -> KeepAliveGuard {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut deadline = first_deadline;
            loop {
                tokio::time::sleep_until(deadline).await;
                let started = tokio::time::Instant::now();
                let mut stream = provider.stream(request.clone());
                // Driven to the end, not dropped at `Completed`: a stream
                // abandoned early is "unconsumed" to the scripted provider,
                // which hands its script back to the next sample.
                let mut billed = None;
                while let Some(ev) = stream.next().await {
                    match ev {
                        Ok(StreamEvent::Completed { usage, .. }) => billed = Some(usage),
                        Ok(StreamEvent::Retrying { .. }) => break,
                        Err(ProviderError::Http { status, .. })
                            if status == 400 || status == 422 =>
                        {
                            rejected.store(true, Ordering::Relaxed);
                            return;
                        }
                        Err(_) => break,
                        Ok(_) => {}
                    }
                }
                if let Some(usage) = billed {
                    let _ = tx.send(usage);
                }
                if rejected.load(Ordering::Relaxed) {
                    return;
                }
                deadline = started + after;
            }
        });
        KeepAliveGuard { task, report: rx }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_provider::ScriptedProvider;
    use hotl_types::StopReason;

    fn req() -> SamplingRequest {
        SamplingRequest {
            model: "claude-opus-5".into(),
            max_tokens: 4096,
            system: "sys".into(),
            items: hotl_provider::arc_items(Vec::new()),
            ephemeral_tail: hotl_provider::arc_items(Vec::new()),
            tools: Arc::from(Vec::new()),
            thinking: true,
            effort: None,
            cache: CachePolicy::Static {
                prefix_ttl: CacheTtl::FiveMinutes,
            },
            turn_context: Some("<moim>".into()),
            cache_key: Some("sid".into()),
        }
    }

    fn one_token() -> Vec<Result<StreamEvent, ProviderError>> {
        vec![
            Ok(StreamEvent::Started { request_id: None }),
            Ok(StreamEvent::Completed {
                stop: StopReason::MaxTokens,
                usage: TokenUsage {
                    cache_read_input_tokens: 100_000,
                    output_tokens: 1,
                    ..Default::default()
                },
                blocks: Vec::new(),
            }),
        ]
    }

    #[test]
    fn projected_saving_is_none_for_an_uncatalogued_model() {
        assert_eq!(projected_saving_usd("sol-fast", 1_000_000), None);
        assert_eq!(ping_cost_usd("sol-fast", 10), None);
    }

    /// Opus 5: write 6.25, read 0.50 per Mtok, one output token 0.000025.
    /// Saving = prefix × (5.75 − 0.50)/1M − 0.000025 → crosses $0.05 near 9.5k.
    #[test]
    fn projected_saving_crosses_the_threshold_near_ten_thousand_opus_5_tokens() {
        let below = projected_saving_usd("claude-opus-5", 9_000).unwrap();
        let above = projected_saving_usd("claude-opus-5", 10_000).unwrap();
        assert!(below < KEEPALIVE_MIN_SAVING_USD, "{below}");
        assert!(above >= KEEPALIVE_MIN_SAVING_USD, "{above}");
        assert!(
            prefix_tokens(&TokenUsage {
                cache_read_input_tokens: 7,
                cache_creation_input_tokens: 5,
                input_tokens: 99,
                ..Default::default()
            }) == 12
        );
    }

    #[test]
    fn the_ping_request_differs_from_the_sample_only_in_max_tokens() {
        let last = req();
        let ping = ping_request(&last);
        assert_eq!(ping.max_tokens, 1);
        // Put the one field back and the two are the same request, byte for
        // byte of their Debug rendering (no Serialize on the type).
        let mut restored = ping.clone();
        restored.max_tokens = last.max_tokens;
        assert_eq!(format!("{restored:?}"), format!("{last:?}"));
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn should_ping_refuses_one_hour_off_disabled_latched_and_over_cap() {
        let five = CachePolicy::Static {
            prefix_ttl: CacheTtl::FiveMinutes,
        };
        let hour = CachePolicy::Static {
            prefix_ttl: CacheTtl::OneHour,
        };
        let rows: [(CachePolicy, bool, bool, Option<f64>, f64, f64, bool); 7] = [
            (five, true, false, Some(0.10), 0.0, 0.0, true),
            (hour, true, false, Some(0.10), 0.0, 0.0, false),
            (CachePolicy::Off, true, false, Some(0.10), 0.0, 0.0, false),
            (five, false, false, Some(0.10), 0.0, 0.0, false),
            (five, true, true, Some(0.10), 0.0, 0.0, false),
            (five, true, false, Some(0.04), 0.0, 0.0, false),
            (five, true, false, Some(0.10), 0.99, 1.0, false),
        ];
        for (i, (cache, on, latched, saving, spent, cap, want)) in rows.into_iter().enumerate() {
            assert_eq!(
                should_ping(cache, on, latched, saving, spent, cap, 0.02),
                want,
                "row {i}"
            );
        }
        assert!(
            !should_ping(five, true, false, None, 0.0, 0.0, 0.02),
            "no price, no ping"
        );
        assert!(
            should_ping(five, true, false, Some(0.10), 0.5, 1.0, 0.02),
            "under the cap"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_ping_fires_at_the_deadline_and_re_arms_from_its_own_start() {
        let provider = Arc::new(ScriptedProvider::new(vec![one_token(), one_token()]));
        let after = Duration::from_secs(255);
        let guard = KeepAlive::arm(
            provider.clone(),
            ping_request(&req()),
            tokio::time::Instant::now() + after,
            after,
            Arc::new(AtomicBool::new(false)),
        );
        tokio::time::advance(Duration::from_secs(254)).await;
        assert_eq!(provider.request_count(), 0);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(provider.request_count(), 1);
        assert_eq!(provider.requests()[0].max_tokens, 1);
        tokio::time::advance(Duration::from_secs(300)).await;
        tokio::task::yield_now().await;
        assert_eq!(provider.request_count(), 2);
        let pings = guard.drain();
        assert_eq!(pings.len(), 2);
        assert_eq!(pings[0].cache_read_input_tokens, 100_000);
    }

    #[tokio::test(start_paused = true)]
    async fn a_ping_that_sees_retrying_gives_up_without_a_second_request() {
        let retrying = vec![Ok(StreamEvent::Retrying {
            attempt: 1,
            max: 5,
            reason: "429".into(),
            delay_ms: 1000,
            status: Some(429),
            request_id: None,
        })];
        let provider = Arc::new(ScriptedProvider::new(vec![retrying]));
        let after = Duration::from_millis(100);
        let rejected = Arc::new(AtomicBool::new(false));
        let guard = KeepAlive::arm(
            provider.clone(),
            ping_request(&req()),
            tokio::time::Instant::now() + after,
            after,
            rejected.clone(),
        );
        tokio::time::advance(Duration::from_millis(150)).await;
        tokio::task::yield_now().await;
        assert_eq!(provider.request_count(), 1);
        assert!(
            !rejected.load(Ordering::Relaxed),
            "a retry is not a rejection"
        );
        // It re-arms from its own start: the next period pings again (and the
        // abandoned script is back at the queue's front, so it sees the same
        // `Retrying`).
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(provider.request_count(), 2);
        assert!(
            guard.drain().is_empty(),
            "a ping that gave up reports no usage"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_400_latches_rejected_and_stops() {
        let bad = vec![Err(ProviderError::Http {
            status: 400,
            message: "max_tokens: 1 is below the minimum".into(),
            retry_after: None,
            request_id: None,
        })];
        let provider = Arc::new(ScriptedProvider::new(vec![bad, one_token()]));
        let after = Duration::from_millis(100);
        let rejected = Arc::new(AtomicBool::new(false));
        let guard = KeepAlive::arm(
            provider.clone(),
            ping_request(&req()),
            tokio::time::Instant::now() + after,
            after,
            rejected.clone(),
        );
        tokio::time::advance(Duration::from_millis(150)).await;
        tokio::task::yield_now().await;
        assert!(rejected.load(Ordering::Relaxed));
        tokio::time::advance(Duration::from_millis(300)).await;
        tokio::task::yield_now().await;
        assert_eq!(provider.request_count(), 1, "latched: no second request");
        assert!(guard.drain().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_guard_aborts_a_sleeping_ping() {
        let provider = Arc::new(ScriptedProvider::new(vec![one_token()]));
        let after = Duration::from_secs(255);
        let guard = KeepAlive::arm(
            provider.clone(),
            ping_request(&req()),
            tokio::time::Instant::now() + after,
            after,
            Arc::new(AtomicBool::new(false)),
        );
        tokio::time::advance(Duration::from_secs(100)).await;
        drop(guard);
        tokio::time::advance(Duration::from_secs(400)).await;
        tokio::task::yield_now().await;
        assert_eq!(provider.request_count(), 0);
    }
}
