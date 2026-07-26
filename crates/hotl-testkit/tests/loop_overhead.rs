//! §S1 loop-overhead CI gate: a multi-sample turn — scripted provider (zero
//! real delay) + a no-op tool + the real `SystemClock` (0011 forbids virtual
//! time on the write path) + the `hotl-store` sync-no-op seam — gated on
//! relative regression against the committed baseline at
//! `../loop-baseline.json`.
//!
//! `overhead = (BoundaryEnd−BoundaryStart) − stream − tools` per sample
//! (`hotl_engine::ledger`); this test reads the flushed `LedgerSummary`'s
//! `overhead_p50_ns`/`overhead_p99_ns` directly rather than recomputing them.
//!
//! Regenerate the baseline after an intentional, understood change to the
//! boundary mechanics the ledger prices (a new machine, a real perf win, a
//! deliberate tradeoff) — never to silence a regression you haven't
//! diagnosed:
//!
//! ```text
//! HOTL_UPDATE_LOOP_BASELINE=1 cargo test -p hotl-testkit --test loop_overhead
//! ```

use futures_util::future::BoxFuture;
use hotl_engine::{EngineConfig, Outcome};
use hotl_provider::ScriptedProvider;
use hotl_testkit::Harness;
use hotl_tools::{Permission, Registry, Tool, ToolOutcome};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Tool round-trips the scripted turn drives, plus one closing text reply —
/// several samples (design doc: "a multi-sample turn"), comfortably under
/// the ledger's fixed 64-sample-per-turn cap (`hotl_engine::ledger`), and
/// enough for a stable nearest-rank p50/p99.
const TOOL_ROUND_TRIPS: usize = 20;

/// Shared-runner-variance tolerance, NOT a perf target: how many multiples
/// of the committed baseline a measured p50/p99 may reach before the gate
/// fails.
const REGRESSION_BAND_MULTIPLIER: u64 = 3;

/// Absolute noise floors (nanoseconds), also shared-runner-variance
/// tolerance, not perf targets: a regression smaller than this never fails
/// the gate regardless of the ratio to baseline, so jitter on an
/// already-tiny baseline can't read as a "multiple of nothing" failure.
const NOISE_FLOOR_P50_NS: u64 = 200_000; // 200µs
const NOISE_FLOOR_P99_NS: u64 = 1_000_000; // 1ms

/// The design doc's advisory absolute budgets (§S1). Printed with a
/// pass/exceed status below, never asserted — an absolute assertion would be
/// flaky under shared-runner variance, which is exactly why the gate itself
/// is relative-to-baseline instead.
const ADVISORY_P50_NS: u64 = 300_000; // 300µs
const ADVISORY_P99_NS: u64 = 2_000_000; // 2ms

fn baseline_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("loop-baseline.json")
}

fn read_baseline() -> (u64, u64) {
    let raw = std::fs::read_to_string(baseline_path()).unwrap_or_else(|e| {
        panic!(
            "no committed baseline at {:?} ({e}); generate one with \
             `HOTL_UPDATE_LOOP_BASELINE=1 cargo test -p hotl-testkit --test loop_overhead`",
            baseline_path()
        )
    });
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parse loop-baseline.json");
    let p50 = v["overhead_p50_ns"].as_u64().expect("overhead_p50_ns");
    let p99 = v["overhead_p99_ns"].as_u64().expect("overhead_p99_ns");
    (p50, p99)
}

fn write_baseline(p50_ns: u64, p99_ns: u64) {
    let v = json!({
        "overhead_p50_ns": p50_ns,
        "overhead_p99_ns": p99_ns,
    });
    let text = serde_json::to_string_pretty(&v).expect("serialize baseline");
    std::fs::write(baseline_path(), format!("{text}\n")).expect("write loop-baseline.json");
}

fn advisory_status(measured: u64, budget: u64) -> &'static str {
    if measured <= budget {
        "PASS"
    } else {
        "EXCEED"
    }
}

/// Does nothing: a zero-cost tool so the measured overhead prices only the
/// loop's own boundary mechanics, never any tool's own work.
struct NoOpTool;

impl Tool for NoOpTool {
    fn name(&self) -> &'static str {
        "noop"
    }
    fn description(&self) -> &str {
        "does nothing; exists to isolate loop overhead from tool work"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission(&self, _input: &serde_json::Value) -> Permission {
        Permission::None
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn run<'a>(
        &'a self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async { ToolOutcome::ok("") })
    }
}

fn registry() -> Registry {
    let mut reg = Registry::builtin();
    reg.register(Box::new(NoOpTool));
    reg
}

#[tokio::test]
async fn loop_overhead_stays_within_the_regression_band() {
    let mut scripts: Vec<_> = (0..TOOL_ROUND_TRIPS)
        .map(|i| ScriptedProvider::tool_call(&format!("t{i}"), "noop", json!({})))
        .collect();
    scripts.push(ScriptedProvider::text_reply("done"));

    let mut h = Harness::with_registry_sync_noop(
        scripts,
        EngineConfig {
            max_turns: TOOL_ROUND_TRIPS as i64 + 5,
            ..Default::default()
        },
        registry(),
    );
    let outcome = h.prompt_and_wait("drive several no-op rounds").await;
    assert!(
        matches!(outcome, Outcome::Done { .. }),
        "scenario must finish clean: {outcome:?}"
    );
    assert_eq!(h.ledger_reports.len(), 1, "one turn, one flush");
    let report = &h.ledger_reports[0];
    assert_eq!(
        report.sample_count,
        TOOL_ROUND_TRIPS + 1,
        "every scripted sample must be reflected in the ledger"
    );

    let measured_p50 = report.overhead_p50_ns;
    let measured_p99 = report.overhead_p99_ns;
    println!(
        "loop overhead: p50={measured_p50}ns (advisory {ADVISORY_P50_NS}ns: {}), \
         p99={measured_p99}ns (advisory {ADVISORY_P99_NS}ns: {})",
        advisory_status(measured_p50, ADVISORY_P50_NS),
        advisory_status(measured_p99, ADVISORY_P99_NS),
    );

    if std::env::var_os("HOTL_UPDATE_LOOP_BASELINE").is_some() {
        write_baseline(measured_p50, measured_p99);
        println!(
            "HOTL_UPDATE_LOOP_BASELINE=1: wrote {:?} (p50={measured_p50}ns p99={measured_p99}ns)",
            baseline_path()
        );
        return;
    }

    let (baseline_p50, baseline_p99) = read_baseline();

    let p50_regression = measured_p50.saturating_sub(baseline_p50);
    let p50_over_band = measured_p50 > baseline_p50.saturating_mul(REGRESSION_BAND_MULTIPLIER);
    assert!(
        !(p50_over_band && p50_regression >= NOISE_FLOOR_P50_NS),
        "p50 loop overhead regressed: measured={measured_p50}ns baseline={baseline_p50}ns \
         (exceeds {REGRESSION_BAND_MULTIPLIER}x baseline AND the {NOISE_FLOOR_P50_NS}ns floor)"
    );

    let p99_regression = measured_p99.saturating_sub(baseline_p99);
    let p99_over_band = measured_p99 > baseline_p99.saturating_mul(REGRESSION_BAND_MULTIPLIER);
    assert!(
        !(p99_over_band && p99_regression >= NOISE_FLOOR_P99_NS),
        "p99 loop overhead regressed: measured={measured_p99}ns baseline={baseline_p99}ns \
         (exceeds {REGRESSION_BAND_MULTIPLIER}x baseline AND the {NOISE_FLOOR_P99_NS}ns floor)"
    );
}
