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
    /// Accepts image content blocks. hotl cannot send them yet (`Item::User`
    /// is `{ text: String }`) — the flag is here so the day it can, the
    /// per-model gate is data and not a new match arm.
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
    /// Shortest prefix the provider will actually cache. Marking a prefix
    /// shorter than this is a no-op that still pays the write premium — the
    /// request succeeds and `cache_creation_input_tokens` comes back `0`.
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

#[cfg(test)]
mod tests {
    use super::*;

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
