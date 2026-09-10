//! The Layer-B resource governor (`specs/exec-plans/2026-07-23-tier1-index.md`
//! §"Concurrency model: green threads vs. governors vs. thread pools").
//!
//! Tokio green threads (Layer A) are never capped — spawning a task per URL,
//! per file, per child is cheap and is exactly what expresses the work's
//! parallelism. What overwhelms a machine is the scarce resource *behind*
//! each task: a concurrent LLM call, an open socket, a forked subprocess.
//! `SessionConcurrency` caps only those three choke points, each with its own
//! semaphore so unrelated lanes never serialize against each other.
//!
//! Exactly one `SessionConcurrency` exists per process: built once (from
//! `[concurrency]` config + `HOTL_CONCURRENCY_*` env, env > config > the
//! fixed default below) and cloned — the clone shares the same `Arc`
//! semaphores, not a fresh independent budget — into every registry/builder
//! that needs it, so parent + every child draw from one shared pool.
//!
//! This module was originally built ahead of the subagent plan that
//! ordinarily introduces it, so `web_fetch`'s concurrent multi-URL fetch had
//! somewhere real to acquire a `request()` permit. `agent()` is now consumed
//! by `spawn` (`hotl::spawn::SpawnTool`, held for a child's whole lifetime,
//! acquired right before `ChildBuilder::build`/`build_fork`) — the runaway
//! sub-agent-spawn guard. `subproc()` is drawn per executed tool call by the
//! engine's batch dispatch, so a 40-call batch never forks 40 children.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How deep the queue in front of the `agents` permit may get, as a multiple
/// of the width itself (0058 T7). Queueing is the *point* of a governor, so
/// this is generous — but unbounded queueing turns a runaway fan-out into a
/// silent hang, where the model waits on children that will not start for
/// minutes and cannot tell that from work in progress. Past the cap the
/// answer is a refusal the model can act on.
pub const AGENT_QUEUE_FACTOR: usize = 4;

/// The `agents` queue is full: `queued` waiting against `cap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentQueueFull {
    pub queued: usize,
    pub cap: usize,
}

/// The three governed resources. Deliberately small and fixed — not
/// `num_cpus` — because concurrent LLM calls and subprocesses cost money,
/// hit rate limits, and consume OS resources; a 32-core box must not default
/// to 32 concurrent model sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrencyLimits {
    /// Concurrent sub-agent LLM sessions.
    pub agents: usize,
    /// Concurrent `web_fetch`/`web_search` HTTP requests.
    pub requests: usize,
    /// Concurrent `bash`/`grep`/hook child processes. Drawn per executed
    /// tool call; a tool that awaits a nested session takes none, or a small
    /// budget would deadlock the child inside its parent's permit.
    pub subprocs: usize,
}

impl Default for ConcurrencyLimits {
    fn default() -> Self {
        Self {
            agents: 4,
            requests: 4,
            subprocs: 8,
        }
    }
}

/// A `Clone` handle onto the one process-wide budget: cloning bumps
/// `Arc` refcounts, it does not create a second, independent set of
/// semaphores.
#[derive(Clone)]
pub struct SessionConcurrency {
    agents: Arc<Semaphore>,
    /// The configured `agents` width, kept because a `Semaphore` reports only
    /// what is *available* — the queue cap is a multiple of the width.
    agents_width: usize,
    /// Children admitted and waiting for an `agents` permit. Shared with
    /// every clone, like the semaphores beside it.
    agents_queued: Arc<AtomicUsize>,
    /// Callers waiting on a `subprocs` permit right now (0061 T17).
    subprocs_queued: Arc<AtomicUsize>,
    requests: Arc<Semaphore>,
    subprocs: Arc<Semaphore>,
}

/// A fresh, independent budget at the default limits. Production builds
/// exactly one `SessionConcurrency` in `agent.rs` and clones it — this is for
/// tests and standalone embedders that have no scaffold to clone from.
impl Default for SessionConcurrency {
    fn default() -> Self {
        Self::new(ConcurrencyLimits::default())
    }
}

impl SessionConcurrency {
    /// `0` on any limit is clamped to `1` — the budget must never be able to
    /// deadlock a caller that awaits a permit no one can ever release.
    pub fn new(limits: ConcurrencyLimits) -> Self {
        let mk = |n: usize| Arc::new(Semaphore::new(n.max(1)));
        Self {
            agents: mk(limits.agents),
            agents_width: limits.agents.max(1),
            agents_queued: Arc::new(AtomicUsize::new(0)),
            subprocs_queued: Arc::new(AtomicUsize::new(0)),
            requests: mk(limits.requests),
            subprocs: mk(limits.subprocs),
        }
    }

    /// [`Self::agent`], but refusing rather than queueing without bound past
    /// [`AGENT_QUEUE_FACTOR`] × the configured width. The error carries both
    /// numbers so the refusal can name them.
    pub async fn agent_queued(&self) -> Result<OwnedSemaphorePermit, AgentQueueFull> {
        let cap = self.agents_width * AGENT_QUEUE_FACTOR;
        let queued = self.agents_queued.fetch_add(1, Ordering::SeqCst) + 1;
        if queued > cap {
            self.agents_queued.fetch_sub(1, Ordering::SeqCst);
            return Err(AgentQueueFull { queued, cap });
        }
        let permit = self.agent().await;
        self.agents_queued.fetch_sub(1, Ordering::SeqCst);
        Ok(permit)
    }

    /// Acquire one of the `agents` permits. `await` here *paces* (queues)
    /// rather than errors — nothing is dropped, just delayed until a permit
    /// frees. Acquire late and narrow: right before the costly step (the LLM
    /// call), so prep work stays concurrent and only the true choke point
    /// queues.
    pub async fn agent(&self) -> OwnedSemaphorePermit {
        self.agents.clone().acquire_owned().await.unwrap()
    }

    /// Acquire one of the `requests` permits (a `web_fetch`/`web_search`
    /// socket).
    pub async fn request(&self) -> OwnedSemaphorePermit {
        self.requests.clone().acquire_owned().await.unwrap()
    }

    /// Acquire one of the `subprocs` permits (a `bash`/`grep`/hook child
    /// process).
    pub async fn subproc(&self) -> OwnedSemaphorePermit {
        self.subprocs.clone().acquire_owned().await.unwrap()
    }

    /// A `subprocs` permit if one is free right now (0061 T17). `None` means
    /// the call is about to wait, which is the surface's cue to say so.
    pub fn try_subproc(&self) -> Option<OwnedSemaphorePermit> {
        self.subprocs.clone().try_acquire_owned().ok()
    }

    /// Enter the `subprocs` queue. The handle reports how many callers were
    /// already waiting *before* anything is awaited — which is what lets a
    /// surface announce the wait before it starts. Process-wide: children
    /// draw from the same budget, so the depth counts every waiter in the
    /// process, not just this session's.
    pub fn subproc_queued(&self) -> SubprocQueue {
        let ahead = self.subprocs_queued.fetch_add(1, Ordering::SeqCst);
        SubprocQueue {
            ahead,
            queued: Arc::clone(&self.subprocs_queued),
            subprocs: Arc::clone(&self.subprocs),
        }
    }
}

/// A place in the `subprocs` queue (0061 T17). Leaves the queue on drop, so
/// a cancelled wait is not counted forever.
pub struct SubprocQueue {
    /// Callers already waiting when this one joined.
    pub ahead: usize,
    queued: Arc<AtomicUsize>,
    subprocs: Arc<Semaphore>,
}

impl SubprocQueue {
    pub async fn acquire(self) -> OwnedSemaphorePermit {
        self.subprocs.clone().acquire_owned().await.unwrap()
    }
}

impl Drop for SubprocQueue {
    fn drop(&mut self) {
        self.queued.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0061 T17: `try_subproc` is what tells a call it is about to wait, and
    /// the queue handle reports the depth it found — before anything awaits,
    /// so the announcement can precede the wait.
    #[tokio::test]
    async fn try_subproc_fails_when_exhausted_and_subproc_queued_reports_waiters_ahead() {
        let c = SessionConcurrency::new(ConcurrencyLimits {
            agents: 1,
            requests: 1,
            subprocs: 1,
        });
        let held = c.try_subproc().expect("the only permit is free");
        assert!(c.try_subproc().is_none(), "the budget is exhausted");

        let first = c.subproc_queued();
        assert_eq!(first.ahead, 0, "nobody was waiting yet");
        let second = c.subproc_queued();
        assert_eq!(second.ahead, 1, "one caller was already in the queue");

        // Leaving the queue without acquiring must not leak the count.
        drop(second);
        let third = c.subproc_queued();
        assert_eq!(third.ahead, 1, "the abandoned wait left the queue");
        drop(third);

        drop(held);
        let _permit = first.acquire().await;
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn default_limits_are_small_and_fixed() {
        let d = ConcurrencyLimits::default();
        assert_eq!(
            d,
            ConcurrencyLimits {
                agents: 4,
                requests: 4,
                subprocs: 8
            }
        );
    }

    #[tokio::test]
    async fn zero_limits_clamp_to_one_and_never_deadlock() {
        let sc = SessionConcurrency::new(ConcurrencyLimits {
            agents: 0,
            requests: 0,
            subprocs: 0,
        });
        // A permit is still obtainable — clamped to 1, not 0 (which would
        // make every `acquire` hang forever).
        let permit = tokio::time::timeout(Duration::from_secs(1), sc.request())
            .await
            .expect("must not deadlock on a zero-configured limit");
        drop(permit);
    }

    #[tokio::test]
    async fn requests_budget_caps_concurrency_at_the_configured_width() {
        let sc = SessionConcurrency::new(ConcurrencyLimits {
            agents: 4,
            requests: 2,
            subprocs: 8,
        });
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let sc = sc.clone();
            let in_flight = in_flight.clone();
            let max_seen = max_seen.clone();
            set.spawn(async move {
                let _permit = sc.request().await;
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            });
        }
        while set.join_next().await.is_some() {}
        // Never more than the configured width held a permit at once, but
        // every task still ran to completion (Layer A stays uncapped).
        assert!(
            max_seen.load(Ordering::SeqCst) <= 2,
            "budget of 2 was exceeded: saw {}",
            max_seen.load(Ordering::SeqCst)
        );
    }

    /// 0058 T7: queueing is the point, but an unbounded queue turns a
    /// runaway fan-out into a silent hang. Past `agents × 4` waiting, the
    /// next arrival is refused with both numbers.
    #[tokio::test]
    async fn the_agent_queue_refuses_past_four_times_the_width() {
        let sc = SessionConcurrency::new(ConcurrencyLimits {
            agents: 1,
            requests: 4,
            subprocs: 8,
        });
        // The one runner, plus four that queue behind it.
        let running = sc.agent_queued().await.expect("the first runs");
        let mut waiting = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let sc = sc.clone();
            waiting.spawn(async move { sc.agent_queued().await.map(drop) });
        }
        // Let all four register as queued before the fifth arrives.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let refused = sc.agent_queued().await;
        assert_eq!(
            refused.err(),
            Some(AgentQueueFull { queued: 5, cap: 4 }),
            "the fifth queued child must be refused, not queued"
        );
        drop(running);
        while let Some(j) = waiting.join_next().await {
            assert!(j.expect("task").is_ok(), "every queued child still runs");
        }
        // The queue drained, so the next arrival is admitted again.
        assert!(sc.agent_queued().await.is_ok());
    }

    #[tokio::test]
    async fn a_clone_shares_the_same_semaphores_not_a_fresh_budget() {
        let sc = SessionConcurrency::new(ConcurrencyLimits {
            agents: 4,
            requests: 1,
            subprocs: 8,
        });
        let clone = sc.clone();
        let held = sc.request().await; // the only permit, held via the original handle
                                       // The clone must see the *same* pool as exhausted — a fresh
                                       // independent semaphore would let this succeed immediately.
        let blocked = tokio::time::timeout(Duration::from_millis(50), clone.request()).await;
        assert!(
            blocked.is_err(),
            "clone acquired a permit the original was holding"
        );
        drop(held);
        // Freed on drop: the clone can now acquire promptly.
        let now_free = tokio::time::timeout(Duration::from_millis(200), clone.request()).await;
        assert!(
            now_free.is_ok(),
            "permit was not released for the clone after drop"
        );
    }
}
