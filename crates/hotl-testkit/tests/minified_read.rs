//! The savings claim, measured end-to-end through the real engine.
//!
//! Every other minified-read test asserts a property of the view. This one
//! asserts the reason the feature exists: the tool result the model receives
//! estimates smaller than the file it describes, on the same estimator the
//! engine budgets context with (`hotl_context::tokens::estimate_text`).
//!
//! It lives here because `Harness` consumers do — nothing outside hotl-testkit
//! depends on hotl-testkit.
//!
//! The threshold is deliberately loose. This pins "savings are real and
//! measured", not an exact ratio that drifts with every separator tweak.

#![cfg(feature = "minify")]

use hotl_engine::{EngineConfig, Outcome};
use hotl_provider::ScriptedProvider;
use hotl_testkit::{tool_batch, Harness};
use hotl_tools::{diagnostics::Diagnostics, MinifyConfig, Registry};
use hotl_types::Item;
use serde_json::json;

fn cfg() -> EngineConfig {
    EngineConfig {
        model: "test-model".into(),
        ..Default::default()
    }
}

/// ~30KB of generated but real-shaped Rust: doc-commented fn blocks.
fn generated_rust() -> String {
    (0..400)
        .map(|i| {
            format!(
                "/// doc for f{i}\nfn f{i}(a: u32, b: u32) -> u32 {{\n    let x = a + b;\n    \
                 x * {i}\n}}\n\n"
            )
        })
        .collect()
}

/// Run one scripted `read` over the fixture and return what the model was
/// served.
///
/// The path is absolute because the read tool resolves against the process cwd,
/// not the harness dir — which makes it an out-of-tree read, gated and approved
/// exactly as the existing harness scenarios establish.
async fn served_by_read(minified: bool) -> (Harness, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("gen.rs");
    std::fs::write(&file, generated_rust()).expect("fixture");
    let mut input = json!({"path": file.to_str().expect("utf8 path")});
    if minified {
        input["minified"] = json!(true);
    }
    let mut h = Harness::with_registry(
        vec![
            tool_batch(&[("t1", "read", input)]),
            ScriptedProvider::text_reply("done"),
        ],
        cfg(),
        Registry::builtin_with(Diagnostics::default(), MinifyConfig::default()),
    );
    h.keep_dir(dir);
    assert_eq!(
        h.prompt_and_wait("summarize gen.rs").await,
        Outcome::Done {
            text: "done".into()
        }
    );
    let served = h
        .items()
        .iter()
        .filter_map(|i| match i {
            Item::ToolResults { results } => Some(results.clone()),
            _ => None,
        })
        .flatten()
        .find(|r| r.tool_use_id == "t1")
        .map(|r| r.content)
        .expect("a tool result for t1");
    (h, served)
}

#[tokio::test]
async fn a_minified_read_saves_tokens_end_to_end_and_lands_in_the_transcript() {
    let (h, served) = served_by_read(true).await;

    // The trailer reaches the model, so the saving is visible, not just real.
    assert!(
        served.contains("[minified view"),
        "trailer visible to the model: {}",
        &served[..served.len().min(200)]
    );
    assert!(
        h.transcript().contains("minified view"),
        "and it is journalled"
    );

    // The measurable claim, on the engine's own estimator.
    let raw = hotl_context::tokens::estimate_text(&generated_rust());
    let after = hotl_context::tokens::estimate_text(&served);
    assert!(
        after < raw * 4 / 5,
        "expected >20% estimator savings; raw={raw} served={after}"
    );
}

#[tokio::test]
async fn the_plain_read_is_unchanged_by_the_minified_mode_existing() {
    let (_h, served) = served_by_read(false).await;
    // `cat -n` prefixes and the truncation trailer, exactly as before.
    assert!(
        served.contains("     1\t/// doc for f0"),
        "{}",
        &served[..served.len().min(80)]
    );
    assert!(
        served.contains("[truncated: showing lines"),
        "the plain path still pages"
    );
    assert!(
        !served.contains("minified"),
        "no minified machinery leaked into the plain view"
    );
}

/// The whole-file contract, seen from the engine: a minified read that also
/// asks to be paged is refused with advice rather than quietly served.
#[tokio::test]
async fn a_minified_read_with_offset_is_refused_through_the_engine() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("gen.rs");
    std::fs::write(&file, generated_rust()).expect("fixture");
    let mut h = Harness::with_registry(
        vec![
            tool_batch(&[(
                "t1",
                "read",
                json!({"path": file.to_str().unwrap(), "minified": true, "offset": 50}),
            )]),
            ScriptedProvider::text_reply("done"),
        ],
        cfg(),
        Registry::builtin_with(Diagnostics::default(), MinifyConfig::default()),
    );
    h.keep_dir(dir);
    h.prompt_and_wait("page through gen.rs").await;
    let errored = h.items().iter().any(|i| match i {
        Item::ToolResults { results } => results
            .iter()
            .any(|r| r.is_error && r.content.contains("plain read")),
        _ => false,
    });
    assert!(errored, "the refusal reaches the model as an error result");
}
