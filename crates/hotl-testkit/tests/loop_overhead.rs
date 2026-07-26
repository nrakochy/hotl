//! §S1 loop-overhead CI gate: a multi-sample turn — scripted provider (zero
//! real delay) + a no-op tool + the real `SystemClock` (0011 forbids virtual
//! time on the write path) + the `hotl-store` sync-no-op seam — gated on
//! relative regression against the committed baseline at
//! `../loop-baseline.json`.
//!
//! `overhead = (BoundaryEnd−BoundaryStart) − stream − tools` per sample
//! (`hotl_engine::ledger`); each trial reads the flushed `LedgerSummary`'s
//! `overhead_p50_ns`/`overhead_p99_ns` directly rather than recomputing them.
//!
//! **Why trials, not one turn's numbers directly**: nearest-rank percentile
//! at `n` samples uses `rank = ceil(p/100 * n)`
//! (`hotl_engine::ledger::percentile`), which resolves to `n` — the single
//! largest sample — for p99 at any `n < 100`. `TOOL_ROUND_TRIPS + 1` samples
//! is nowhere near that, so a single turn's "p99" is really just that turn's
//! one biggest sample: a single rare scheduling hiccup on ONE sample decides
//! the whole number. The gate instead runs `TRIAL_COUNT` independent turns
//! and compares the *median* of their p50s/p99s — moving that requires
//! roughly half the trials to independently spike, while a systematic
//! regression (this gate's actual job) still shifts every trial and moves
//! the median just as visibly (`gate_would_catch_a_real_regression` checks
//! exactly that).
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

/// Tool round-trips one trial's scripted turn drives, plus one closing text
/// reply — several samples (design doc: "a multi-sample turn"), comfortably
/// under the ledger's fixed 64-sample-per-turn cap (`hotl_engine::ledger`).
const TOOL_ROUND_TRIPS: usize = 20;

/// Independent trials the main gate takes the median over. Robust to a
/// single trial's outlier draw (see the module doc comment); odd, so
/// `median` always has one exact middle element, no interpolation.
const TRIAL_COUNT: usize = 15;

/// Trials for `gate_would_catch_a_real_regression`. Fewer than `TRIAL_COUNT`
/// because that check's regression (no sync-noop seam: real `sync_data()`
/// on every sample) is a huge, systematic shift — tens of milliseconds
/// against a sub-millisecond baseline — not a noise-sensitive measurement,
/// so it doesn't need the same statistical care. Odd for the same reason as
/// `TRIAL_COUNT`.
const TEETH_CHECK_TRIALS: usize = 5;

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

/// Median across independent trials — not averaged (deliberately exact for
/// integer nanosecond values). `values` must be non-empty; callers use an
/// odd trial count so there's always a single middle element after sorting.
fn median(values: &mut [u64]) -> u64 {
    assert!(!values.is_empty(), "median of zero trials is undefined");
    values.sort_unstable();
    values[values.len() / 2]
}

/// The spec-mandated regression check, shared by the gate assertion and its
/// own teeth check so the two can never drift apart: a regression counts
/// only past BOTH the relative band and the absolute noise floor.
fn exceeds_band(measured: u64, baseline: u64, multiplier: u64, floor: u64) -> bool {
    let over_band = measured > baseline.saturating_mul(multiplier);
    let regression = measured.saturating_sub(baseline);
    over_band && regression >= floor
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

/// Drive one independent trial of the scripted scenario —
/// `TOOL_ROUND_TRIPS` no-op tool round-trips plus a closing text reply — and
/// return `(overhead_p50_ns, overhead_p99_ns)` off the turn's single
/// flushed `LedgerReport`. `use_seam` selects the sync-no-op seam (the
/// gate's own scenario) or real `sync_data()` (the teeth check's deliberate,
/// large regression).
async fn run_scenario(use_seam: bool) -> (u64, u64) {
    let mut scripts: Vec<_> = (0..TOOL_ROUND_TRIPS)
        .map(|i| ScriptedProvider::tool_call(&format!("t{i}"), "noop", json!({})))
        .collect();
    scripts.push(ScriptedProvider::text_reply("done"));

    let config = EngineConfig {
        max_turns: TOOL_ROUND_TRIPS as i64 + 5,
        ..Default::default()
    };
    let mut h = if use_seam {
        Harness::with_registry_sync_noop(scripts, config, registry())
    } else {
        Harness::with_registry(scripts, config, registry())
    };
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
    (report.overhead_p50_ns, report.overhead_p99_ns)
}

#[tokio::test]
async fn loop_overhead_stays_within_the_regression_band() {
    let mut p50s = Vec::with_capacity(TRIAL_COUNT);
    let mut p99s = Vec::with_capacity(TRIAL_COUNT);
    for _ in 0..TRIAL_COUNT {
        let (p50, p99) = run_scenario(true).await;
        p50s.push(p50);
        p99s.push(p99);
    }
    let median_p50 = median(&mut p50s);
    let median_p99 = median(&mut p99s);

    println!(
        "loop overhead (median of {TRIAL_COUNT} trials): \
         p50={median_p50}ns (advisory {ADVISORY_P50_NS}ns: {}), \
         p99={median_p99}ns (advisory {ADVISORY_P99_NS}ns: {})",
        advisory_status(median_p50, ADVISORY_P50_NS),
        advisory_status(median_p99, ADVISORY_P99_NS),
    );

    if std::env::var_os("HOTL_UPDATE_LOOP_BASELINE").is_some() {
        write_baseline(median_p50, median_p99);
        println!(
            "HOTL_UPDATE_LOOP_BASELINE=1: wrote {:?} (median of {TRIAL_COUNT} trials: \
             p50={median_p50}ns p99={median_p99}ns)",
            baseline_path()
        );
        return;
    }

    let (baseline_p50, baseline_p99) = read_baseline();

    assert!(
        !exceeds_band(
            median_p50,
            baseline_p50,
            REGRESSION_BAND_MULTIPLIER,
            NOISE_FLOOR_P50_NS
        ),
        "p50 loop overhead regressed: median-of-{TRIAL_COUNT}-trials={median_p50}ns \
         baseline={baseline_p50}ns (exceeds {REGRESSION_BAND_MULTIPLIER}x baseline AND \
         the {NOISE_FLOOR_P50_NS}ns floor)"
    );
    assert!(
        !exceeds_band(
            median_p99,
            baseline_p99,
            REGRESSION_BAND_MULTIPLIER,
            NOISE_FLOOR_P99_NS
        ),
        "p99 loop overhead regressed: median-of-{TRIAL_COUNT}-trials={median_p99}ns \
         baseline={baseline_p99}ns (exceeds {REGRESSION_BAND_MULTIPLIER}x baseline AND \
         the {NOISE_FLOOR_P99_NS}ns floor)"
    );
}

/// Automated teeth check: a real regression — the sync-no-op seam disabled,
/// so every sample pays a real `sync_data()` — must still trip the gate's
/// own regression check against the committed baseline. Guards against the
/// median-of-trials fix (above) smoothing away a genuine regression along
/// with the noise it was built to absorb.
#[tokio::test]
async fn gate_would_catch_a_real_regression() {
    let mut p50s = Vec::with_capacity(TEETH_CHECK_TRIALS);
    let mut p99s = Vec::with_capacity(TEETH_CHECK_TRIALS);
    for _ in 0..TEETH_CHECK_TRIALS {
        let (p50, p99) = run_scenario(false).await;
        p50s.push(p50);
        p99s.push(p99);
    }
    let median_p50 = median(&mut p50s);
    let median_p99 = median(&mut p99s);
    let (baseline_p50, baseline_p99) = read_baseline();

    assert!(
        exceeds_band(
            median_p50,
            baseline_p50,
            REGRESSION_BAND_MULTIPLIER,
            NOISE_FLOOR_P50_NS
        ),
        "a real regression (sync-noop seam disabled) must trip the p50 gate: \
         median-of-{TEETH_CHECK_TRIALS}-trials={median_p50}ns baseline={baseline_p50}ns"
    );
    assert!(
        exceeds_band(
            median_p99,
            baseline_p99,
            REGRESSION_BAND_MULTIPLIER,
            NOISE_FLOOR_P99_NS
        ),
        "a real regression (sync-noop seam disabled) must trip the p99 gate: \
         median-of-{TEETH_CHECK_TRIALS}-trials={median_p99}ns baseline={baseline_p99}ns"
    );
}
