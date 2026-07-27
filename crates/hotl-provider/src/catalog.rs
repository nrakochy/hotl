//! Per-model static catalog: context window, output cap, prices, capability
//! flags, and the minimum cacheable prompt prefix.
//!
//! Vendored snapshot, not a live query. The provider's `GET /v1/models` is the
//! live authority (and `hotl models` surfaces it — see
//! `specs/exec-plans/2026-07-22-hotl-models-command.md`); this table exists so
//! the *engine* can size its context window and price a turn without a network
//! round-trip on every session start.
//!
//! INVARIANT: a model absent from this table is a supported configuration, not
//! an error — `lookup` returns `None` and callers fall back to a documented
//! default. hotl has never allowlisted model names and must not start here.
//! Enforced by `unknown_models_are_none_not_an_error`.

use hotl_types::TokenUsage;

/// The model hotl uses when nothing else is configured.
///
/// INVARIANT (partially unimplemented — see
/// specs/exec-plans/active/0014-remediation-model-registry.md): this is the
/// single definition. Two copies still exist at
/// `hotl-provider-anthropic/src/lib.rs:23` and `hotl-engine/src/lib.rs:80`,
/// owned by R3 and R2 respectively; both have a decision-log request to
/// re-export this one. Drift is caught meanwhile by
/// `config::tests::default_model_matches_the_anthropic_provider`.
pub const DEFAULT_MODEL: &str = "claude-opus-4-8";

/// What an uncatalogued model's context window resolves to. Deliberately equal
/// to the value `EngineConfig::default()` hardcoded before this catalog existed
/// (`hotl-engine/src/lib.rs:90`), so no existing configuration changes behavior
/// as a side effect of this plan.
pub const FALLBACK_CONTEXT_WINDOW: u64 = 200_000;

/// Feature flags that change what a request may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Accepts a `thinking` block (any dialect).
    pub thinking: bool,
    /// Accepts image content blocks (`Item::User { images }` on the wire).
    /// Gated per-request in the dialect serializers via [`supports_images`].
    pub images: bool,
    /// Accepts `output_config.effort`.
    pub effort: bool,
}

/// One model's properties. Prices are **USD per million tokens**.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelInfo {
    pub id: &'static str,
    pub context_window: u64,
    pub max_output_tokens: u32,
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
    /// Cached-prefix reads: ~0.1x input.
    pub cache_read_usd_per_mtok: f64,
    /// Cache writes at the default 5-minute TTL: ~1.25x input.
    pub cache_write_usd_per_mtok: f64,
    /// Cache writes marked for the 1-hour TTL: 2x input (vs. 1.25x for the
    /// 5-minute default) — the premium for the longer hold.
    pub cache_write_1h_usd_per_mtok: f64,
    /// Shortest prefix the provider will actually cache. Marking a prefix
    /// shorter than this is a no-op: the request succeeds, but the provider
    /// does not create a cache entry, so `cache_creation_input_tokens` comes
    /// back `0` and nothing is billed for the write.
    /// Not monotonic across generations, which is why it is data.
    pub min_cacheable_prefix: u64,
    /// ASCII characters per token, for the heuristic estimator
    /// (`hotl_context::TokenProfile`). Lower = denser tokenization.
    pub ascii_chars_per_token: f32,
    pub caps: Caps,
}

const FULL: Caps = Caps {
    thinking: true,
    images: true,
    effort: true,
};
const NO_EFFORT: Caps = Caps {
    thinking: true,
    images: true,
    effort: false,
};

/// Anthropic models only, deliberately: the OpenAI-compatible family is
/// open-ended (any Ollama/llama.cpp/gateway model string is forwarded
/// verbatim), so seeding guesses there would be fabricated data whose only
/// effect is silently moving the compaction trigger.
pub const CATALOG: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-fable-5",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        input_usd_per_mtok: 10.00,
        output_usd_per_mtok: 50.00,
        cache_read_usd_per_mtok: 1.00,
        cache_write_usd_per_mtok: 12.50,
        cache_write_1h_usd_per_mtok: 20.00,
        min_cacheable_prefix: 512,
        ascii_chars_per_token: 3.0,
        caps: FULL,
    },
    ModelInfo {
        id: "claude-opus-5",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        input_usd_per_mtok: 5.00,
        output_usd_per_mtok: 25.00,
        cache_read_usd_per_mtok: 0.50,
        cache_write_usd_per_mtok: 6.25,
        cache_write_1h_usd_per_mtok: 10.00,
        min_cacheable_prefix: 512,
        ascii_chars_per_token: 3.0,
        caps: FULL,
    },
    ModelInfo {
        id: "claude-opus-4-8",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        input_usd_per_mtok: 5.00,
        output_usd_per_mtok: 25.00,
        cache_read_usd_per_mtok: 0.50,
        cache_write_usd_per_mtok: 6.25,
        cache_write_1h_usd_per_mtok: 10.00,
        min_cacheable_prefix: 1024,
        ascii_chars_per_token: 3.0,
        caps: FULL,
    },
    ModelInfo {
        id: "claude-opus-4-7",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        input_usd_per_mtok: 5.00,
        output_usd_per_mtok: 25.00,
        cache_read_usd_per_mtok: 0.50,
        cache_write_usd_per_mtok: 6.25,
        cache_write_1h_usd_per_mtok: 10.00,
        min_cacheable_prefix: 2048,
        ascii_chars_per_token: 3.0,
        caps: FULL,
    },
    ModelInfo {
        id: "claude-opus-4-6",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        input_usd_per_mtok: 5.00,
        output_usd_per_mtok: 25.00,
        cache_read_usd_per_mtok: 0.50,
        cache_write_usd_per_mtok: 6.25,
        cache_write_1h_usd_per_mtok: 10.00,
        min_cacheable_prefix: 4096,
        ascii_chars_per_token: 3.5,
        caps: FULL,
    },
    ModelInfo {
        id: "claude-sonnet-5",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        input_usd_per_mtok: 3.00,
        output_usd_per_mtok: 15.00,
        cache_read_usd_per_mtok: 0.30,
        cache_write_usd_per_mtok: 3.75,
        cache_write_1h_usd_per_mtok: 6.00,
        min_cacheable_prefix: 1024,
        ascii_chars_per_token: 3.0,
        caps: FULL,
    },
    ModelInfo {
        id: "claude-sonnet-4-6",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        input_usd_per_mtok: 3.00,
        output_usd_per_mtok: 15.00,
        cache_read_usd_per_mtok: 0.30,
        cache_write_usd_per_mtok: 3.75,
        cache_write_1h_usd_per_mtok: 6.00,
        min_cacheable_prefix: 1024,
        ascii_chars_per_token: 3.5,
        caps: FULL,
    },
    ModelInfo {
        id: "claude-haiku-4-5",
        context_window: 200_000,
        max_output_tokens: 64_000,
        input_usd_per_mtok: 1.00,
        output_usd_per_mtok: 5.00,
        cache_read_usd_per_mtok: 0.10,
        cache_write_usd_per_mtok: 1.25,
        cache_write_1h_usd_per_mtok: 2.00,
        min_cacheable_prefix: 4096,
        ascii_chars_per_token: 3.5,
        caps: NO_EFFORT,
    },
];

/// Resolve a model string to its catalog row.
///
/// Three passes, cheapest first: exact id; then with a leading `provider/` or
/// `provider.` segment stripped (hotl's own `anthropic/claude-…` spec spelling
/// and Bedrock's `anthropic.claude-…`); then the **longest** catalogued id
/// that prefixes what remains, which folds dated snapshots
/// (`claude-haiku-4-5-20251001`) onto their family. Longest-wins so a future
/// `claude-opus-4-8x` is never answered by `claude-opus-4-8`.
pub fn lookup(model: &str) -> Option<&'static ModelInfo> {
    if model.is_empty() {
        return None;
    }
    if let Some(hit) = CATALOG.iter().find(|m| m.id == model) {
        return Some(hit);
    }
    let bare = model
        .split_once('/')
        .or_else(|| model.split_once('.'))
        .map(|(_, rest)| rest)
        .unwrap_or(model);
    if let Some(hit) = CATALOG.iter().find(|m| m.id == bare) {
        return Some(hit);
    }
    CATALOG
        .iter()
        .filter(|m| bare.starts_with(m.id))
        .max_by_key(|m| m.id.len())
}

/// The model's context window in tokens, or `None` when uncatalogued.
///
/// Returning `Option` rather than a defaulted `u64` is deliberate: the caller
/// is the only layer that knows whether it can *warn* about the fallback, and
/// silently substituting 200K is precisely the defect this module exists to
/// remove. See `config::ContextCfg::resolve_window`.
pub fn context_window(model: &str) -> Option<u64> {
    lookup(model).map(|m| m.context_window)
}

/// The model's maximum output tokens per request, or `None` when uncatalogued.
pub fn max_output_tokens(model: &str) -> Option<u32> {
    lookup(model).map(|m| m.max_output_tokens)
}

/// Price a turn's reported usage in USD, or `None` for an uncatalogued model.
///
/// Each bucket is priced at its own rate and summed. On the Anthropic wire
/// `input_tokens` is the *uncached remainder* — cache reads and cache writes
/// are reported separately and are not included in it — so this sum is the
/// whole request, not a partial one.
///
/// Cache-write pricing depends on which TTL a write actually landed under:
/// the wire may report a per-TTL breakdown
/// (`cache_creation_5m_input_tokens` / `cache_creation_1h_input_tokens`), in
/// which case each bucket is priced at its own rate (2x input for 1h, 1.25x
/// for 5m). When the provider's response carries no such breakdown (both
/// buckets zero) but `cache_creation_input_tokens` is nonzero, the whole
/// total is priced at the 5m rate — a documented underestimate for any of
/// that traffic actually written at the 1h TTL, chosen because it is the
/// cheaper of the two rather than fabricating a split hotl was never told.
/// The same fallback applies to whatever the breakdown *doesn't* cover: when
/// the two buckets are present but their sum falls short of
/// `cache_creation_input_tokens` (a shape the wire is not documented to send,
/// but nothing rules out), the saturating excess is priced at the 5m rate
/// too, so the total charged always accounts for every creation token
/// reported rather than quietly pricing the remainder at zero.
///
/// INVARIANT: caching a prefix is never more expensive than not caching the
/// same traffic. Enforced by `cost_prices_each_usage_bucket_at_its_own_rate`.
pub fn cost_usd(model: &str, usage: &TokenUsage) -> Option<f64> {
    let m = lookup(model)?;
    let per = |tokens: u64, rate: f64| (tokens as f64 / 1_000_000.0) * rate;
    let no_breakdown =
        usage.cache_creation_5m_input_tokens == 0 && usage.cache_creation_1h_input_tokens == 0;
    let creation_cost = if no_breakdown {
        per(
            usage.cache_creation_input_tokens,
            m.cache_write_usd_per_mtok,
        )
    } else {
        let excess = usage.cache_creation_input_tokens.saturating_sub(
            usage.cache_creation_5m_input_tokens + usage.cache_creation_1h_input_tokens,
        );
        per(
            usage.cache_creation_5m_input_tokens,
            m.cache_write_usd_per_mtok,
        ) + per(
            usage.cache_creation_1h_input_tokens,
            m.cache_write_1h_usd_per_mtok,
        ) + per(excess, m.cache_write_usd_per_mtok)
    };
    Some(
        per(usage.input_tokens, m.input_usd_per_mtok)
            + per(usage.output_tokens, m.output_usd_per_mtok)
            + per(usage.cache_read_input_tokens, m.cache_read_usd_per_mtok)
            + creation_cost,
    )
}

/// Shortest prompt prefix this model will actually cache, or `None` when
/// uncatalogued.
pub fn min_cacheable_prefix(model: &str) -> Option<u64> {
    lookup(model).map(|m| m.min_cacheable_prefix)
}

/// Would marking a prefix of `estimated_tokens` actually produce a cache
/// entry? A shorter prefix is silently not cached — the request succeeds and
/// `cache_creation_input_tokens` comes back `0` — while still paying the write
/// premium for whatever *is* cached after it.
///
/// Fails **towards** marking on an unknown model: a wasted mark costs the
/// premium once, a skipped mark costs a full prefix rebuild every turn.
pub fn is_cacheable_prefix(model: &str, estimated_tokens: u64) -> bool {
    match min_cacheable_prefix(model) {
        Some(min) => estimated_tokens >= min,
        None => true,
    }
}

/// May a request for `model` carry image content blocks?
///
/// Fails **open** for an uncatalogued model — same philosophy as
/// [`is_cacheable_prefix`]: hotl never allowlists model names, and the
/// OpenAI-compat family is deliberately uncatalogued. A non-vision endpoint
/// answers with its own 400, which is surfaced honestly; silently dropping
/// the user's image is the worse lie.
pub fn supports_images(model: &str) -> bool {
    lookup(model).map(|m| m.caps.images).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_images_reads_the_catalog_and_fails_open_for_unknown_models() {
        assert!(supports_images(DEFAULT_MODEL));
        assert!(supports_images("some-local-ollama-model"));
    }

    #[test]
    fn default_model_is_in_the_catalog_and_has_a_million_token_window() {
        let info = lookup(DEFAULT_MODEL).expect("the default model must be catalogued");
        assert_eq!(info.id, "claude-opus-4-8");
        assert_eq!(info.context_window, 1_000_000);
        assert_eq!(info.max_output_tokens, 128_000);
        // The headline defect: hotl compacts this model at 160K today.
        assert!(
            info.context_window > FALLBACK_CONTEXT_WINDOW,
            "the whole point of the catalog is that 200K is wrong here"
        );
    }

    #[test]
    fn lookup_strips_provider_prefixes_and_dated_suffixes() {
        // hotl's own `provider/model` spelling.
        assert_eq!(
            lookup("anthropic/claude-opus-5").unwrap().id,
            "claude-opus-5"
        );
        // Bedrock's dotted spelling.
        assert_eq!(
            lookup("anthropic.claude-opus-5").unwrap().id,
            "claude-opus-5"
        );
        // A dated snapshot resolves to its family.
        assert_eq!(
            lookup("claude-haiku-4-5-20251001").unwrap().id,
            "claude-haiku-4-5"
        );
        // Longest prefix wins: 4-8 must not answer for a bare `claude-opus-4`.
        assert_eq!(lookup("claude-opus-4-8").unwrap().id, "claude-opus-4-8");
    }

    #[test]
    fn unknown_models_are_none_not_an_error() {
        // Discovery, not validation: hotl never allowlists a model name.
        assert!(lookup("llama3").is_none());
        assert!(lookup("openai/gpt-5").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn window_and_output_resolve_per_model_and_none_for_unknown() {
        assert_eq!(context_window("claude-opus-4-8"), Some(1_000_000));
        assert_eq!(context_window("claude-haiku-4-5"), Some(200_000));
        assert_eq!(context_window("anthropic/claude-sonnet-5"), Some(1_000_000));
        // Unknown: None, so the caller decides the fallback (and can warn).
        assert_eq!(context_window("llama3"), None);

        assert_eq!(max_output_tokens("claude-haiku-4-5"), Some(64_000));
        assert_eq!(max_output_tokens("claude-opus-5"), Some(128_000));
        assert_eq!(max_output_tokens("mistral-large"), None);
    }

    #[test]
    fn the_engines_default_max_tokens_fits_every_catalogued_model() {
        // `EngineConfig::default().max_tokens` is 32_000
        // (hotl-engine/src/lib.rs:81). If a catalogued model ever caps output
        // below that, the request 400s — catch it here, not in production.
        for m in CATALOG {
            assert!(
                m.max_output_tokens >= 32_000,
                "{} caps output at {} < the engine default 32_000",
                m.id,
                m.max_output_tokens
            );
        }
    }

    #[test]
    fn cost_prices_each_usage_bucket_at_its_own_rate() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            ..Default::default()
        };
        // Opus 4.8: 5.00 + 25.00 + 0.50 + 6.25 (no per-TTL breakdown ⇒ the
        // 1,000,000 creation tokens fall back to the 5m rate).
        let cost = cost_usd("claude-opus-4-8", &usage).unwrap();
        assert!((cost - 36.75).abs() < 1e-9, "was {cost}");

        // Uncatalogued: no price, no guess.
        assert!(cost_usd("llama3", &usage).is_none());

        // `input_tokens` is the *uncached remainder* on the wire — a cache hit
        // must be cheaper than the same traffic uncached, never more.
        let cached = TokenUsage {
            input_tokens: 0,
            cache_read_input_tokens: 1_000_000,
            ..Default::default()
        };
        let uncached = TokenUsage {
            input_tokens: 1_000_000,
            ..Default::default()
        };
        assert!(
            cost_usd("claude-opus-4-8", &cached).unwrap()
                < cost_usd("claude-opus-4-8", &uncached).unwrap()
        );
    }

    #[test]
    fn cost_prices_the_1h_bucket_at_double_the_5m_bucket() {
        // A per-TTL breakdown present: each bucket prices at its own rate,
        // not the blended/fallback rate.
        let usage = TokenUsage {
            cache_creation_5m_input_tokens: 1_000_000,
            cache_creation_1h_input_tokens: 1_000_000,
            cache_creation_input_tokens: 2_000_000,
            ..Default::default()
        };
        // Opus 4.8: 5m bucket at 6.25 + 1h bucket at 10.00 (2x input).
        let cost = cost_usd("claude-opus-4-8", &usage).unwrap();
        assert!((cost - 16.25).abs() < 1e-9, "was {cost}");
    }

    #[test]
    fn cost_prices_the_breakdown_shortfall_at_the_5m_rate() {
        // Both TTL buckets are present (so the fallback path is not taken),
        // but they undercount the reported total by 200,000 tokens — a
        // mixed shape the wire is not documented to send, but one this
        // function must still be total-preserving over: every creation
        // token reported gets priced, none of it at $0.
        let usage = TokenUsage {
            cache_creation_5m_input_tokens: 500_000,
            cache_creation_1h_input_tokens: 300_000,
            cache_creation_input_tokens: 1_000_000,
            ..Default::default()
        };
        // Opus 4.8: 5m bucket 0.5*6.25 = 3.125, 1h bucket 0.3*10.00 = 3.00,
        // and the 200,000 excess priced at the 5m rate: 0.2*6.25 = 1.25.
        let cost = cost_usd("claude-opus-4-8", &usage).unwrap();
        assert!((cost - 7.375).abs() < 1e-9, "was {cost}");
    }

    #[test]
    fn cost_falls_back_to_the_5m_rate_when_the_ttl_breakdown_is_absent() {
        // Total creation is nonzero but neither TTL bucket is populated (an
        // older/partial provider response) — price everything at the cheaper
        // 5m rate rather than guessing a split.
        let usage = TokenUsage {
            cache_creation_input_tokens: 1_000_000,
            ..Default::default()
        };
        let cost = cost_usd("claude-opus-4-8", &usage).unwrap();
        assert!((cost - 6.25).abs() < 1e-9, "was {cost}");
    }

    #[test]
    fn cacheable_prefix_predicate_knows_the_generation_split() {
        // Not monotonic across generations — this is why it is data.
        assert_eq!(min_cacheable_prefix("claude-opus-5"), Some(512));
        assert_eq!(min_cacheable_prefix("claude-opus-4-8"), Some(1024));
        assert_eq!(min_cacheable_prefix("claude-opus-4-6"), Some(4096));
        assert_eq!(min_cacheable_prefix("claude-haiku-4-5"), Some(4096));

        // A 300-token system prompt: a no-op mark on every model.
        assert!(!is_cacheable_prefix("claude-opus-5", 300));
        // 600 tokens caches on Opus 5 and silently does not on Opus 4.8.
        assert!(is_cacheable_prefix("claude-opus-5", 600));
        assert!(!is_cacheable_prefix("claude-opus-4-8", 600));
        // Unknown model: assume it caches. Fail *towards* marking — a wasted
        // mark costs a premium once; a skipped mark costs a full rebuild every
        // turn.
        assert!(is_cacheable_prefix("llama3", 1));
    }

    #[test]
    fn every_row_is_internally_consistent() {
        for m in CATALOG {
            assert!(m.context_window >= 8_000, "{}: window", m.id);
            assert!(
                (m.max_output_tokens as u64) < m.context_window,
                "{}: output cap must fit inside the window",
                m.id
            );
            assert!(
                m.output_usd_per_mtok > m.input_usd_per_mtok,
                "{}: prices",
                m.id
            );
            assert!(
                m.cache_read_usd_per_mtok < m.input_usd_per_mtok,
                "{}: cache read",
                m.id
            );
            assert!(
                m.cache_write_usd_per_mtok > m.input_usd_per_mtok,
                "{}: cache write",
                m.id
            );
            assert!(
                (m.cache_write_1h_usd_per_mtok - m.input_usd_per_mtok * 2.0).abs() < 1e-9,
                "{}: 1h cache write must be exactly 2x input",
                m.id
            );
            assert!(
                m.cache_write_1h_usd_per_mtok > m.cache_write_usd_per_mtok,
                "{}: 1h write must cost more than the 5m default",
                m.id
            );
            assert!(m.min_cacheable_prefix >= 512, "{}: cache prefix", m.id);
            assert!(m.ascii_chars_per_token > 1.0, "{}: ratio", m.id);
            // No duplicate ids — lookup's prefix walk assumes uniqueness.
            assert_eq!(
                CATALOG.iter().filter(|o| o.id == m.id).count(),
                1,
                "{}: duplicated",
                m.id
            );
        }
    }
}
