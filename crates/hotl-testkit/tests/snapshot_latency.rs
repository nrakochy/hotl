//! 0035 Task 7 — the latency gate, and the owner directive this plan exists
//! for: time-to-first-tool-dispatch and `TurnDone` are independent of repo
//! size and snapshotter speed. The measured incident was a first bash call
//! stalled ~60s behind an awaited `git add -A .` into an empty shadow index;
//! the double below models exactly that worker (its queue never empties, a
//! drain would eat its whole grace) — and the turn must not care, because
//! nothing in the turn path awaits a snapshot any more.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use hotl_engine::{EngineConfig, Outcome, Snapshotter};
use hotl_provider::ScriptedProvider;
use hotl_testkit::Harness;
use hotl_tools::{Permission, Registry, Tool, ToolOutcome};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// A mutating no-op: `read_only() == false` is what makes the engine fire
/// the taint signal and enqueue the quiet-window snapshot for its batch.
struct MutatingNoOp;

impl Tool for MutatingNoOp {
    fn name(&self) -> &'static str {
        "mutate"
    }
    fn description(&self) -> &str {
        "does nothing, but counts as mutating"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission(&self, _input: &serde_json::Value) -> Permission {
        Permission::None
    }
    fn read_only(&self) -> bool {
        false
    }
    fn run<'a>(
        &'a self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async { ToolOutcome::ok("") })
    }
}

/// The worker analog of a 60s `git add` on a giant repo: enqueue succeeds
/// instantly (the trait demands it), but the queue never empties — a drain
/// eats its entire grace, and any code path that waited on this snapshotter
/// would hang for the full simulated stage.
#[derive(Default)]
struct WedgedWorkerSnapshotter(Mutex<Vec<String>>);

impl Snapshotter for WedgedWorkerSnapshotter {
    fn snapshot(&self, label: String) {
        self.0.lock().unwrap().push(label);
    }
    fn drain(&self, grace: Duration) {
        std::thread::sleep(grace);
    }
}

struct Timings {
    first_dispatch: Duration,
    turn_done: Duration,
    labels: Vec<String>,
}

async fn timed_mutating_turn(snapshotter: Arc<WedgedWorkerSnapshotter>) -> Timings {
    let mut reg = Registry::builtin();
    reg.register(Box::new(MutatingNoOp));
    let scripts = vec![
        ScriptedProvider::tool_call("t1", "mutate", json!({})),
        ScriptedProvider::text_reply("done"),
    ];
    let mut h = Harness::with_registry_and_snapshotter(
        scripts,
        EngineConfig::default(),
        reg,
        Arc::clone(&snapshotter) as Arc<dyn Snapshotter>,
    );
    let start = Instant::now();
    h.handle.prompt("mutate once".to_string()).await;
    let mut first_dispatch = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), h.handle.events.recv())
            .await
            .expect("event timeout — something is waiting on the wedged snapshotter")
            .expect("event channel closed");
        match event {
            hotl_engine::EngineEvent::ToolStart { .. } => {
                first_dispatch.get_or_insert(start.elapsed());
            }
            hotl_engine::EngineEvent::TurnDone { outcome, .. } => {
                assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
                return Timings {
                    first_dispatch: first_dispatch.expect("the tool dispatched"),
                    turn_done: start.elapsed(),
                    labels: snapshotter.0.lock().unwrap().clone(),
                };
            }
            _ => {}
        }
    }
}

/// The acceptance gate: a snapshotter whose worker is wedged for good must
/// leave dispatch and `TurnDone` at the no-snapshotter baseline. The bound is
/// deliberately generous for CI (the failure mode it guards against was 60s
/// per mutating batch, and unbounded in principle); the pre-0035 tree awaited
/// the pre-batch snapshot inline and cannot pass this.
#[tokio::test]
async fn a_wedged_snapshot_worker_never_delays_dispatch_or_turn_done() {
    const BOUND: Duration = Duration::from_secs(5);
    let timings = timed_mutating_turn(Arc::new(WedgedWorkerSnapshotter::default())).await;
    assert!(
        timings.first_dispatch < BOUND,
        "first dispatch took {:?} — a snapshot is gating the tool path",
        timings.first_dispatch
    );
    assert!(
        timings.turn_done < BOUND,
        "TurnDone took {:?} — the turn end is waiting on a snapshot",
        timings.turn_done
    );
    // The capture side still happened: the batch's quiet-window snapshot was
    // enqueued (the wedged worker just hasn't run it — undo is degraded, the
    // turn is not).
    assert_eq!(
        timings.labels,
        ["state after batch 1"],
        "{:?}",
        timings.labels
    );
}
