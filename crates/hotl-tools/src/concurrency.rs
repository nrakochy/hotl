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
//! This module is owned here (built ahead of the subagent plan that
//! ordinarily introduces it) so `web_fetch`'s concurrent multi-URL fetch has
//! somewhere real to acquire a `request()` permit; `agent()`/`subproc()` are
//! unused today and wait for the plans that fan out sub-agents and
//! subprocesses to draw on them.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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
    /// Concurrent `bash`/`grep`/hook child processes.
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
    requests: Arc<Semaphore>,
    subprocs: Arc<Semaphore>,
}

impl SessionConcurrency {
    /// `0` on any limit is clamped to `1` — the budget must never be able to
    /// deadlock a caller that awaits a permit no one can ever release.
    pub fn new(limits: ConcurrencyLimits) -> Self {
        let mk = |n: usize| Arc::new(Semaphore::new(n.max(1)));
        Self {
            agents: mk(limits.agents),
            requests: mk(limits.requests),
            subprocs: mk(limits.subprocs),
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
