//! Per-session registry of sub-agents (0072): what `spawn … background`
//! starts, `wait`/`send`/`resume` reach, and the session closes at its end.
//! No engine dependency — the engine holds one of these per session (the
//! `jobs` shape) and the tools read it through the task-local [`CHILDREN`].
//!
//! A result reaches exactly one reader (D5): the foreground caller, a parked
//! `wait`, or the parent's inbox. The slot's state machine makes that a
//! property of the registry rather than of the task that drives the child:
//! `Running → Finishing → Done { claimed }`. `finish` hands the result to a
//! waiter when one is parked, else returns [`Deliver::Notify`] and leaves the
//! slot `Finishing` until [`Children::published`] says the notification was
//! sent; a waiter that arrives in between is served at publish, and the
//! publisher is told to withdraw its notification.

use std::sync::{Arc, Mutex, PoisonError};

use hotl_types::TokenUsage;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::DelegationFacts;

/// Running plus parked children per session (`[agents] max_children`).
pub const DEFAULT_MAX_CHILDREN: usize = 8;

/// What a child cost its parent (D11): priced and counted on admit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChildCharge {
    pub usage: TokenUsage,
    pub model: String,
    pub tool_calls: u64,
}

/// A finished child's answer: the same text the foreground call returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChildResult {
    pub text: String,
    pub ok: bool,
    pub facts: DelegationFacts,
    pub secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildState {
    Running,
    /// Finished; its notification is being sent.
    Finishing,
    /// Parked: the actor is alive and `resume` reaches it.
    Done {
        ok: bool,
        claimed: bool,
        ended_ms: u64,
    },
    /// Evicted, stopped, or the session ended. Nothing reaches it.
    Closed,
}

/// One row of `/agents` and `session/children`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRecord {
    pub id: String,
    /// The `spawn` call's tool_use id.
    pub call_id: Option<String>,
    pub agent_type: String,
    pub summary: String,
    pub started_ms: u64,
    pub background: bool,
    pub isolated: bool,
    pub state: ChildState,
}

impl ChildRecord {
    /// Wall time so far, or to the end.
    pub fn secs(&self, now_ms: u64) -> u64 {
        let end = match &self.state {
            ChildState::Done { ended_ms, .. } => *ended_ms,
            _ => now_ms,
        };
        end.saturating_sub(self.started_ms) / 1000
    }
}

/// `resume {child, text}` handed to the parked task. `reply` is `Some` for a
/// foreground resume (the caller waits on it); `None` means the result goes
/// through [`Children::finish`] like a background child's.
pub struct ResumeRequest {
    pub text: String,
    pub reply: Option<oneshot::Sender<ChildResult>>,
}

/// What the task registers before it starts.
pub struct ChildSpec {
    pub call_id: Option<String>,
    pub agent_type: String,
    pub summary: String,
    pub background: bool,
    pub isolated: bool,
    pub started_ms: u64,
    pub steer: mpsc::Sender<String>,
    pub resume: mpsc::Sender<ResumeRequest>,
}

/// The registered slot's handles, for the task that drives the child.
#[derive(Debug)]
pub struct ChildEntry {
    pub id: String,
    /// `/agents stop`, eviction: this child alone.
    pub interrupt: CancellationToken,
    /// Session end: every child.
    pub stop_all: CancellationToken,
}

type Waiter = oneshot::Sender<(String, ChildResult)>;

struct ChildSlot {
    record: ChildRecord,
    result: Option<ChildResult>,
    waiter: Option<Waiter>,
    steer: mpsc::Sender<String>,
    resume: mpsc::Sender<ResumeRequest>,
    interrupt: CancellationToken,
}

/// What [`Children::claim`] found.
pub enum Claim {
    Ready(String, ChildResult),
    Parked(oneshot::Receiver<(String, ChildResult)>),
    /// Already being waited on by another call.
    Busy,
    Closed,
    /// No such id; the ids that exist.
    Unknown(Vec<String>),
    /// `wait {}` with nothing running and nothing unclaimed.
    Nothing(Vec<String>),
}

/// Where [`Children::finish`] put the result.
#[derive(Debug, PartialEq, Eq)]
pub enum Deliver {
    ToWaiter,
    /// Nobody waits: notify the parent, then call `published`.
    Notify,
}

/// Why `send` could not reach a child.
#[derive(Debug, PartialEq, Eq)]
pub enum SendRefusal {
    Unknown(Vec<String>),
    /// Finished and parked: `resume` is the verb.
    Parked,
    Closed,
}

/// Why `resume` could not reach a child.
#[derive(Debug, PartialEq, Eq)]
pub enum ResumeRefusal {
    Unknown(Vec<String>),
    /// Still running: `send` is the verb.
    Running,
    Closed,
    /// Its worktree is spent (D7).
    Isolated,
}

struct Inner {
    slots: Vec<ChildSlot>,
    any_waiter: Option<Waiter>,
}

/// The registry. One per session; the actor owns it, the tools borrow it.
pub struct Children {
    inner: Mutex<Inner>,
    stop_all: CancellationToken,
    max: usize,
}

impl std::fmt::Debug for Children {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Children(n={})", self.lock().slots.len())
    }
}

impl Default for Children {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CHILDREN)
    }
}

impl Children {
    pub fn new(max: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                slots: Vec::new(),
                any_waiter: None,
            }),
            stop_all: CancellationToken::new(),
            max: max.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session-wide stop token: a background child drains against this,
    /// never against the turn that started it (D6).
    pub fn token(&self) -> CancellationToken {
        self.stop_all.clone()
    }

    /// Register a child. Past `max`, the oldest parked child is closed first;
    /// when every slot is running, `Err` lists their ids.
    pub fn register(&self, spec: ChildSpec) -> Result<ChildEntry, Vec<String>> {
        let mut inner = self.lock();
        let live = |s: &ChildSlot| s.record.state != ChildState::Closed;
        if inner.slots.iter().filter(|s| live(s)).count() >= self.max {
            let oldest_parked = inner
                .slots
                .iter_mut()
                .filter(|s| matches!(s.record.state, ChildState::Done { .. }))
                .min_by_key(|s| s.record.started_ms);
            match oldest_parked {
                Some(s) => close(s),
                None => {
                    return Err(inner
                        .slots
                        .iter()
                        .filter(|s| live(s))
                        .map(|s| s.record.id.clone())
                        .collect())
                }
            }
        }
        let id = fresh_id(&inner.slots);
        let interrupt = CancellationToken::new();
        inner.slots.push(ChildSlot {
            record: ChildRecord {
                id: id.clone(),
                call_id: spec.call_id,
                agent_type: spec.agent_type,
                summary: spec.summary,
                started_ms: spec.started_ms,
                background: spec.background,
                isolated: spec.isolated,
                state: ChildState::Running,
            },
            result: None,
            waiter: None,
            steer: spec.steer,
            resume: spec.resume,
            interrupt: interrupt.clone(),
        });
        Ok(ChildEntry {
            id,
            interrupt,
            stop_all: self.stop_all.clone(),
        })
    }

    /// `wait`: a finished result at once, or a parked receiver. `None` takes
    /// the oldest unclaimed result, else parks on whichever finishes first.
    pub fn claim(&self, id: Option<&str>) -> Claim {
        let mut inner = self.lock();
        let Some(id) = id else {
            let oldest = inner
                .slots
                .iter_mut()
                .filter(|s| matches!(s.record.state, ChildState::Done { claimed: false, .. }))
                .min_by_key(|s| s.record.started_ms);
            if let Some(s) = oldest {
                return claim_done(s);
            }
            let running = inner
                .slots
                .iter()
                .any(|s| matches!(s.record.state, ChildState::Running | ChildState::Finishing));
            if !running {
                return Claim::Nothing(known(&inner.slots));
            }
            if inner.any_waiter.is_some() {
                return Claim::Busy;
            }
            let (tx, rx) = oneshot::channel();
            inner.any_waiter = Some(tx);
            return Claim::Parked(rx);
        };
        let known = known(&inner.slots);
        let Some(s) = inner.slots.iter_mut().find(|s| s.record.id == id) else {
            return Claim::Unknown(known);
        };
        match s.record.state {
            ChildState::Closed => Claim::Closed,
            ChildState::Done { .. } => claim_done(s),
            ChildState::Running | ChildState::Finishing => {
                if s.waiter.is_some() {
                    return Claim::Busy;
                }
                let (tx, rx) = oneshot::channel();
                s.waiter = Some(tx);
                Claim::Parked(rx)
            }
        }
    }

    /// The child finished. A parked waiter gets the result and the slot is
    /// `Done { claimed: true }`; otherwise the slot holds it as `Finishing`
    /// and the caller must notify, then call [`Self::published`].
    pub fn finish(&self, id: &str, result: ChildResult, now_ms: u64) -> Deliver {
        let mut inner = self.lock();
        let Inner { slots, any_waiter } = &mut *inner;
        let Some(s) = slots.iter_mut().find(|s| s.record.id == id) else {
            return Deliver::ToWaiter;
        };
        let ok = result.ok;
        // A session-wide waiter is served only when no per-id one is.
        let waiter = s.waiter.take().or_else(|| any_waiter.take());
        if let Some(w) = waiter {
            s.record.state = ChildState::Done {
                ok,
                claimed: true,
                ended_ms: now_ms,
            };
            s.result = Some(result.clone());
            if w.send((id.to_string(), result)).is_ok() {
                return Deliver::ToWaiter;
            }
            // The waiter gave up (its turn was cancelled): the inbox it is.
            s.record.state = ChildState::Finishing;
        } else {
            s.record.state = ChildState::Finishing;
            s.result = Some(result);
        }
        Deliver::Notify
    }

    /// The notification is sent: the slot is `Done`. Returns `true` when a
    /// waiter arrived in between and was handed the result here — the caller
    /// must then withdraw its notification (`Unnotify`), which it can do in
    /// channel order because its own `Notify` send already completed.
    pub fn published(&self, id: &str, now_ms: u64) -> bool {
        let mut inner = self.lock();
        let Inner { slots, any_waiter } = &mut *inner;
        let Some(s) = slots.iter_mut().find(|s| s.record.id == id) else {
            return false;
        };
        if s.record.state != ChildState::Finishing {
            return false;
        }
        let ok = s.result.as_ref().is_some_and(|r| r.ok);
        let waiter = s.waiter.take().or_else(|| any_waiter.take());
        let claimed = match (waiter, s.result.clone()) {
            (Some(w), Some(r)) => w.send((id.to_string(), r)).is_ok(),
            _ => false,
        };
        s.record.state = ChildState::Done {
            ok,
            claimed,
            ended_ms: now_ms,
        };
        claimed
    }

    /// A parked child is being resumed: it runs again.
    pub fn resumed(&self, id: &str) {
        if let Some(s) = self.lock().slots.iter_mut().find(|s| s.record.id == id) {
            if matches!(s.record.state, ChildState::Done { .. }) {
                s.record.state = ChildState::Running;
                s.result = None;
            }
        }
    }

    /// `send`: the running child's steer channel.
    pub fn steer(&self, id: &str) -> Result<mpsc::Sender<String>, SendRefusal> {
        let inner = self.lock();
        let Some(s) = inner.slots.iter().find(|s| s.record.id == id) else {
            return Err(SendRefusal::Unknown(known(&inner.slots)));
        };
        match s.record.state {
            ChildState::Running | ChildState::Finishing => Ok(s.steer.clone()),
            ChildState::Done { .. } => Err(SendRefusal::Parked),
            ChildState::Closed => Err(SendRefusal::Closed),
        }
    }

    /// `resume`: the parked child's request channel.
    pub fn resume(&self, id: &str) -> Result<mpsc::Sender<ResumeRequest>, ResumeRefusal> {
        let inner = self.lock();
        let Some(s) = inner.slots.iter().find(|s| s.record.id == id) else {
            return Err(ResumeRefusal::Unknown(known(&inner.slots)));
        };
        match s.record.state {
            ChildState::Running | ChildState::Finishing => Err(ResumeRefusal::Running),
            ChildState::Closed => Err(ResumeRefusal::Closed),
            ChildState::Done { .. } if s.record.isolated => Err(ResumeRefusal::Isolated),
            ChildState::Done { .. } => Ok(s.resume.clone()),
        }
    }

    /// `/agents stop <id>`: interrupt a running child or close a parked one.
    /// `false` when no live child has that id.
    pub fn stop(&self, id: &str) -> bool {
        let mut inner = self.lock();
        match inner.slots.iter_mut().find(|s| s.record.id == id) {
            Some(s) if s.record.state != ChildState::Closed => {
                close(s);
                true
            }
            _ => false,
        }
    }

    /// Session end (D6): every child, running or parked.
    pub fn stop_all(&self) {
        self.stop_all.cancel();
        for s in self.lock().slots.iter_mut() {
            if s.record.state != ChildState::Closed {
                close(s);
            }
        }
    }

    /// The task ended on its own (a panic, a dropped handle): the record says
    /// so instead of staying `Running` forever.
    pub fn abandoned(&self, id: &str, text: String, now_ms: u64) {
        let mut inner = self.lock();
        let Inner { slots, any_waiter } = &mut *inner;
        let Some(s) = slots.iter_mut().find(|s| s.record.id == id) else {
            return;
        };
        if !matches!(s.record.state, ChildState::Running | ChildState::Finishing) {
            return;
        }
        let result = ChildResult {
            text,
            ok: false,
            facts: DelegationFacts::default(),
            secs: now_ms.saturating_sub(s.record.started_ms) / 1000,
        };
        if let Some(w) = s.waiter.take().or_else(|| any_waiter.take()) {
            let _ = w.send((id.to_string(), result.clone()));
        }
        s.result = Some(result);
        s.record.state = ChildState::Closed;
    }

    /// Children still working (the goal gate, D14).
    pub fn running(&self) -> usize {
        self.lock()
            .slots
            .iter()
            .filter(|s| matches!(s.record.state, ChildState::Running | ChildState::Finishing))
            .count()
    }

    pub fn list(&self) -> Vec<ChildRecord> {
        self.lock().slots.iter().map(|s| s.record.clone()).collect()
    }

    pub fn record(&self, id: &str) -> Option<ChildRecord> {
        self.lock()
            .slots
            .iter()
            .find(|s| s.record.id == id)
            .map(|s| s.record.clone())
    }
}

fn close(s: &mut ChildSlot) {
    s.interrupt.cancel();
    s.record.state = ChildState::Closed;
    s.waiter = None;
}

fn claim_done(s: &mut ChildSlot) -> Claim {
    if let ChildState::Done { claimed, .. } = &mut s.record.state {
        *claimed = true;
    }
    match s.result.clone() {
        Some(r) => Claim::Ready(s.record.id.clone(), r),
        None => Claim::Closed,
    }
}

fn known(slots: &[ChildSlot]) -> Vec<String> {
    slots
        .iter()
        .filter(|s| s.record.state != ChildState::Closed)
        .map(|s| s.record.id.clone())
        .collect()
}

/// Six lowercase chars off a fresh ULID (0064's job-id shape); re-drawn on a
/// collision.
fn fresh_id(taken: &[ChildSlot]) -> String {
    loop {
        let ulid = hotl_types::new_ulid().to_ascii_lowercase();
        let id = ulid[ulid.len() - 6..].to_string();
        if !taken.iter().any(|s| s.record.id == id) {
            return id;
        }
    }
}

tokio::task_local! {
    pub static CHILDREN: Arc<Children>;
}

/// The registry of the session whose tool call runs on this task; `None`
/// outside a session (then `background`, `wait`, `send`, `resume` are tool
/// errors).
pub fn registry() -> Option<Arc<Children>> {
    CHILDREN.try_with(Clone::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(background: bool) -> ChildSpec {
        let (steer, _) = mpsc::channel(1);
        let (resume, _) = mpsc::channel(1);
        ChildSpec {
            call_id: Some("t1".into()),
            agent_type: "explore".into(),
            summary: "look".into(),
            background,
            isolated: false,
            started_ms: 1_000,
            steer,
            resume,
        }
    }

    fn result(text: &str) -> ChildResult {
        ChildResult {
            text: text.into(),
            ok: true,
            facts: DelegationFacts::default(),
            secs: 3,
        }
    }

    #[test]
    fn a_parked_waiter_is_handed_the_result_and_no_notify_is_due() {
        let c = Children::new(8);
        let e = c.register(spec(true)).unwrap();
        let Claim::Parked(rx) = c.claim(Some(&e.id)) else {
            panic!("a running child parks the waiter");
        };
        assert_eq!(c.finish(&e.id, result("done"), 5_000), Deliver::ToWaiter);
        let (id, got) = rx.blocking_recv().unwrap();
        assert_eq!((id.as_str(), got.text.as_str()), (e.id.as_str(), "done"));
        assert_eq!(
            c.record(&e.id).unwrap().state,
            ChildState::Done {
                ok: true,
                claimed: true,
                ended_ms: 5_000
            }
        );
        assert_eq!(c.running(), 0);
    }

    #[test]
    fn finish_without_a_waiter_returns_notify_and_the_slot_stays_finishing_until_published() {
        let c = Children::new(8);
        let e = c.register(spec(true)).unwrap();
        assert_eq!(c.finish(&e.id, result("done"), 5_000), Deliver::Notify);
        assert_eq!(c.record(&e.id).unwrap().state, ChildState::Finishing);
        assert_eq!(c.running(), 1, "finishing still holds the goal gate");
        assert!(!c.published(&e.id, 5_001), "nobody arrived meanwhile");
        assert_eq!(
            c.record(&e.id).unwrap().state,
            ChildState::Done {
                ok: true,
                claimed: false,
                ended_ms: 5_001
            }
        );
        assert_eq!(c.running(), 0);
    }

    #[test]
    fn a_waiter_that_arrives_while_finishing_is_served_at_publish_and_owes_an_unnotify() {
        let c = Children::new(8);
        let e = c.register(spec(true)).unwrap();
        assert_eq!(c.finish(&e.id, result("done"), 5_000), Deliver::Notify);
        let Claim::Parked(rx) = c.claim(Some(&e.id)) else {
            panic!("a finishing slot parks, never hands out a result the inbox may get");
        };
        assert!(c.published(&e.id, 5_001), "the waiter took it: unnotify");
        assert_eq!(rx.blocking_recv().unwrap().1.text, "done");
        assert!(matches!(
            c.record(&e.id).unwrap().state,
            ChildState::Done { claimed: true, .. }
        ));
    }

    #[test]
    fn claim_on_a_done_slot_marks_it_claimed_once() {
        let c = Children::new(8);
        let e = c.register(spec(true)).unwrap();
        c.finish(&e.id, result("done"), 5_000);
        c.published(&e.id, 5_000);
        let Claim::Ready(id, r) = c.claim(Some(&e.id)) else {
            panic!("a done slot answers at once");
        };
        assert_eq!((id, r.text.as_str()), (e.id.clone(), "done"));
        assert!(matches!(
            c.record(&e.id).unwrap().state,
            ChildState::Done { claimed: true, .. }
        ));
        // Asking again is the model's own duplicate, not a lost result.
        assert!(matches!(c.claim(Some(&e.id)), Claim::Ready(..)));
        assert!(matches!(c.claim(None), Claim::Nothing(ids) if ids == vec![e.id.clone()]));
    }

    #[test]
    fn claim_with_no_id_takes_the_oldest_unclaimed() {
        let c = Children::new(8);
        let mut older = spec(true);
        older.started_ms = 100;
        let a = c.register(older).unwrap();
        let b = c.register(spec(true)).unwrap();
        c.finish(&b.id, result("b"), 5_000);
        c.published(&b.id, 5_000);
        c.finish(&a.id, result("a"), 6_000);
        c.published(&a.id, 6_000);
        let Claim::Ready(id, _) = c.claim(None) else {
            panic!()
        };
        assert_eq!(id, a.id, "oldest by start, not by finish");
        let Claim::Ready(id, _) = c.claim(None) else {
            panic!()
        };
        assert_eq!(id, b.id);
        // Nothing unclaimed and one running: park on any.
        let r = c.register(spec(true)).unwrap();
        let Claim::Parked(rx) = c.claim(None) else {
            panic!("parks on whichever finishes first")
        };
        assert!(matches!(c.claim(None), Claim::Busy));
        assert_eq!(c.finish(&r.id, result("r"), 7_000), Deliver::ToWaiter);
        assert_eq!(rx.blocking_recv().unwrap().0, r.id);
    }

    #[test]
    fn the_registry_refuses_a_ninth_running_child_and_evicts_the_oldest_parked_one() {
        let c = Children::new(8);
        let ids: Vec<String> = (0..8)
            .map(|i| {
                let mut s = spec(true);
                s.started_ms = 100 + i;
                c.register(s).unwrap().id
            })
            .collect();
        let refused = c.register(spec(true)).unwrap_err();
        assert_eq!(
            refused.len(),
            8,
            "every slot is running: refused, ids named"
        );
        // Park two; the ninth evicts the older parked one, never a running one.
        c.finish(&ids[3], result("x"), 5_000);
        c.published(&ids[3], 5_000);
        c.finish(&ids[1], result("y"), 5_000);
        c.published(&ids[1], 5_000);
        let ninth = c.register(spec(true)).unwrap();
        assert_eq!(c.record(&ids[1]).unwrap().state, ChildState::Closed);
        assert!(matches!(
            c.record(&ids[3]).unwrap().state,
            ChildState::Done { .. }
        ));
        assert_eq!(c.running(), 7);
        assert!(matches!(c.resume(&ids[1]), Err(ResumeRefusal::Closed)));
        assert!(matches!(c.claim(Some(&ids[1])), Claim::Closed));
        assert!(c.resume(&ids[3]).is_ok());
        assert!(matches!(c.resume(&ninth.id), Err(ResumeRefusal::Running)));
        assert!(matches!(c.steer(&ids[3]), Err(SendRefusal::Parked)));
        assert!(c.steer(&ninth.id).is_ok());
    }

    #[test]
    fn stop_all_cancels_every_running_child_token() {
        let c = Children::new(8);
        let a = c.register(spec(true)).unwrap();
        let b = c.register(spec(false)).unwrap();
        c.finish(&b.id, result("b"), 5_000);
        c.published(&b.id, 5_000);
        assert!(!a.stop_all.is_cancelled());
        c.stop_all();
        assert!(a.stop_all.is_cancelled() && b.stop_all.is_cancelled());
        assert!(a.interrupt.is_cancelled() && b.interrupt.is_cancelled());
        assert!(c.list().iter().all(|r| r.state == ChildState::Closed));
        assert_eq!(c.running(), 0);
        // `stop` reaches one child by id; a closed one is not found.
        assert!(!c.stop(&a.id));
        let d = c.register(spec(true)).unwrap();
        assert!(c.stop(&d.id) && d.interrupt.is_cancelled());
        assert!(matches!(c.steer(&d.id), Err(SendRefusal::Closed)));
    }

    #[test]
    fn an_isolated_parked_child_is_not_resumable() {
        let c = Children::new(8);
        let mut s = spec(true);
        s.isolated = true;
        let e = c.register(s).unwrap();
        c.finish(&e.id, result("x"), 5_000);
        c.published(&e.id, 5_000);
        assert!(matches!(c.resume(&e.id), Err(ResumeRefusal::Isolated)));
        assert!(
            matches!(c.resume("zzzzzz"), Err(ResumeRefusal::Unknown(ids)) if ids == vec![e.id.clone()])
        );
        assert!(matches!(c.steer("zzzzzz"), Err(SendRefusal::Unknown(_))));
        assert!(matches!(c.claim(Some("zzzzzz")), Claim::Unknown(_)));
    }

    #[test]
    fn ids_are_six_lowercase_chars_and_unique() {
        let c = Children::new(64);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            let id = c.register(spec(true)).unwrap().id;
            assert_eq!(id.len(), 6, "{id}");
            assert!(
                id.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()),
                "{id}"
            );
            assert!(seen.insert(id));
        }
        assert_eq!(c.record("nope"), None);
        let rec = c.list()[0].clone();
        assert_eq!(rec.secs(4_000), 3);
    }

    #[test]
    fn an_abandoned_child_closes_as_an_error_and_wakes_its_waiter() {
        let c = Children::new(8);
        let e = c.register(spec(true)).unwrap();
        let Claim::Parked(rx) = c.claim(Some(&e.id)) else {
            panic!()
        };
        c.abandoned(&e.id, "the task panicked".into(), 9_000);
        let (_, r) = rx.blocking_recv().unwrap();
        assert!(!r.ok && r.text.contains("panicked"));
        assert_eq!(c.record(&e.id).unwrap().state, ChildState::Closed);
        assert_eq!(c.running(), 0);
    }
}
