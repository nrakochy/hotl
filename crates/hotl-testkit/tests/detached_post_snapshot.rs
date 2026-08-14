//! 0033 Task 4: the post-batch shadow snapshot is capture-only — nothing
//! in-session ever reads it — so it must not gate the next sample. These
//! scenarios hold a "post"-labelled snapshot open and prove the turn keeps
//! moving, then prove the turn's end still waits for it (no half-taken
//! snapshot may outlive its turn).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use hotl_engine::{EngineConfig, Outcome, Snapshotter};
use hotl_provider::ScriptedProvider;
use hotl_testkit::Harness;
use hotl_tools::{Permission, Registry, Tool, ToolOutcome};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// A mutating no-op: `read_only() == false` is what makes the engine take
/// the pre/post snapshot pair around its batch.
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

/// Shared state of the gate: labels seen, the release signal, and the two
/// observability flags the assertions read.
struct Gate {
    labels: Mutex<Vec<String>>,
    release: tokio::sync::Notify,
    post_started: AtomicBool,
    post_finished: AtomicBool,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            labels: Mutex::new(Vec::new()),
            release: tokio::sync::Notify::new(),
            post_started: AtomicBool::new(false),
            post_finished: AtomicBool::new(false),
        })
    }
}

/// Records labels; "post"-labelled snapshots block until the gate releases.
struct GatedPostSnapshotter(Arc<Gate>);

impl Snapshotter for GatedPostSnapshotter {
    fn snapshot(&self, label: String) -> BoxFuture<'static, ()> {
        self.0.labels.lock().unwrap().push(label.clone());
        if !label.starts_with("post") {
            return Box::pin(async {});
        }
        let gate = Arc::clone(&self.0);
        Box::pin(async move {
            gate.post_started.store(true, Ordering::SeqCst);
            gate.release.notified().await;
            gate.post_finished.store(true, Ordering::SeqCst);
        })
    }
}

fn registry() -> Registry {
    let mut reg = Registry::builtin();
    reg.register(Box::new(MutatingNoOp));
    reg
}

/// The overlap itself: with the post-snap still held open, the next sample's
/// request must go out and stream — the first `TextDelta` after `ToolDone`
/// proves `RequestBuilt` happened while the snapshot was pending. Turn end
/// then joins the snapshot: `TurnDone` only after it finished.
#[tokio::test]
async fn the_post_batch_snapshot_does_not_gate_the_next_sample() {
    let gate = Gate::new();
    let scripts = vec![
        ScriptedProvider::tool_call("t1", "mutate", json!({})),
        ScriptedProvider::text_reply("done"),
    ];
    let mut h = Harness::with_registry_and_snapshotter(
        scripts,
        EngineConfig::default(),
        registry(),
        Arc::new(GatedPostSnapshotter(Arc::clone(&gate))),
    );
    h.handle.prompt("mutate once".to_string()).await;
    let mut saw_tool_done = false;
    let mut released = false;
    let outcome = loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(10), h.handle.events.recv())
                .await
                .expect("event timeout — the post snapshot is gating the next sample")
                .expect("event channel closed");
        match event {
            hotl_engine::EngineEvent::ToolDone { .. } => saw_tool_done = true,
            hotl_engine::EngineEvent::TextDelta(_) if saw_tool_done && !released => {
                released = true;
                // Sample 2 is streaming. The old awaited post-snap could not
                // have let us get here: its future is still blocked.
                assert!(
                    gate.post_started.load(Ordering::SeqCst),
                    "the post snapshot must have been requested"
                );
                assert!(
                    !gate.post_finished.load(Ordering::SeqCst),
                    "sample 2 streamed strictly before the post snapshot resolved"
                );
                gate.release.notify_one();
            }
            hotl_engine::EngineEvent::TurnDone { outcome, .. } => break outcome,
            _ => {}
        }
    };
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    // Turn end joined the detached snapshot before reporting.
    assert!(
        gate.post_finished.load(Ordering::SeqCst),
        "TurnDone must not outrun a pending post snapshot"
    );
    let labels = gate.labels.lock().unwrap().clone();
    assert_eq!(labels, ["pre batch 1", "post batch 1"], "{labels:?}");
}

/// The serialization half: the next *pre* snapshot (one shadow index) waits
/// for a still-running post snapshot instead of overlapping it.
#[tokio::test]
async fn the_next_pre_snapshot_waits_for_a_pending_post() {
    let gate = Gate::new();
    let scripts = vec![
        ScriptedProvider::tool_call("t1", "mutate", json!({})),
        ScriptedProvider::tool_call("t2", "mutate", json!({})),
        ScriptedProvider::text_reply("done"),
    ];
    let mut h = Harness::with_registry_and_snapshotter(
        scripts,
        EngineConfig::default(),
        registry(),
        Arc::new(GatedPostSnapshotter(Arc::clone(&gate))),
    );
    h.handle.prompt("mutate twice".to_string()).await;
    let mut tool_dones = 0;
    let outcome = loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(10), h.handle.events.recv())
                .await
                .expect("event timeout")
                .expect("event channel closed");
        match event {
            hotl_engine::EngineEvent::ToolStart { .. } if tool_dones == 1 => {
                // Batch 2 is starting, so its pre-snap already ran — which is
                // only possible after batch 1's post-snap fully resolved.
                assert!(
                    gate.post_finished.load(Ordering::SeqCst),
                    "pre batch 2 must have joined the pending post batch 1"
                );
            }
            hotl_engine::EngineEvent::ToolDone { .. } => {
                tool_dones += 1;
                // Batch 1's post snapshot is about to be requested (or just
                // was); let it resolve so pre batch 2 can proceed — but only
                // after the join point actually waits on it.
                if tool_dones == 1 {
                    gate.release.notify_one();
                }
                if tool_dones == 2 {
                    gate.release.notify_one();
                }
            }
            hotl_engine::EngineEvent::TurnDone { outcome, .. } => break outcome,
            _ => {}
        }
    };
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    let labels = gate.labels.lock().unwrap().clone();
    assert_eq!(
        labels,
        ["pre batch 1", "post batch 1", "pre batch 2", "post batch 2"],
        "snapshot order must serialize on the one shadow index"
    );
}
