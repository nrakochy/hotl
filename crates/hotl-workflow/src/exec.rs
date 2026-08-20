//! The executor: runs a validated [`Plan`] through an [`AgentRunner`] with two
//! semaphores (the process-wide gate and the run's own width), the agent-start
//! cap, per-item pipelining without barriers, votes, `until_quiet` rounds, and
//! cancellation. Schema retry is the runner's job; this only passes `schema`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::future::{join_all, BoxFuture};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::plan::{AgentSpec, Effort, Isolation, Phase, Plan, PlanError, Shape, UntilQuiet};
use crate::select::{Lookup, SelectError, Selector};
use crate::template::{Template, TemplateError};

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRequest {
    /// `<run_id>:<phase>:<label>:<n>` — distinct per start, even for reused labels.
    pub id: String,
    pub phase: String,
    pub label: String,
    pub prompt: String,
    pub schema: Option<Value>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub isolation: Option<Isolation>,
    pub max_turns: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentReply {
    pub value: Result<Value, String>,
    /// input + output + cache_read + cache_creation, when the runner knows.
    pub tokens: Option<u64>,
    /// hotl-authored remark about the agent (a kept worktree path, say).
    pub note: Option<String>,
}

pub trait AgentRunner: Send + Sync {
    fn run(&self, req: AgentRequest, cancel: CancellationToken) -> BoxFuture<'_, AgentReply>;
}

/// Progress sink. Sync and must not block — the host forwards with `try_send`
/// or a spawned task, so a slow UI never stalls the scheduler.
pub trait Observer: Send + Sync {
    fn started(&self, id: &str, phase: &str, label: &str);
    fn finished(&self, id: &str, ok: bool, tokens: Option<u64>);
    fn note(&self, text: &str);
}

/// The observer that observes nothing.
pub struct Silent;

impl Observer for Silent {
    fn started(&self, _: &str, _: &str, _: &str) {}
    fn finished(&self, _: &str, _: bool, _: Option<u64>) {}
    fn note(&self, _: &str) {}
}

/// `[workflows]` limits: a plan can lower either, never raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub concurrency: usize,
    pub max_agents: usize,
}

pub struct Run<'a> {
    pub plan: &'a Plan,
    pub args: Value,
    pub run_id: String,
    pub limits: Limits,
    /// Process-wide width, shared by every run in the process.
    pub gate: Arc<Semaphore>,
    pub cancel: CancellationToken,
    pub summary: Arc<Mutex<RunSummary>>,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum RunError {
    #[error("the plan is invalid:\n{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))]
    Invalid(Vec<PlanError>),
    #[error("Workflow cancelled after {finished} of {started} agents")]
    Cancelled { finished: usize, started: usize },
    #[error("phase `{phase}` would start more than {cap} agents (the `max_agents` cap)")]
    AgentCap { phase: String, cap: usize },
    #[error("phase `{phase}`: {source}")]
    Select {
        phase: String,
        #[source]
        source: SelectError,
    },
    #[error("phase `{phase}` agent `{label}`: {source}")]
    Template {
        phase: String,
        label: String,
        #[source]
        source: TemplateError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Done => "done",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Running => "running",
            AgentStatus::Done => "done",
            AgentStatus::Failed => "failed",
            AgentStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentRecord {
    pub id: String,
    pub label: String,
    pub status: AgentStatus,
    pub tokens: Option<u64>,
    pub started: Instant,
    pub settled: Option<Instant>,
    pub error: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PhaseSummary {
    pub title: String,
    pub agents: Vec<AgentRecord>,
}

/// What `/workflows` reads, live: phases → agents → status/tokens/timing.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub id: String,
    pub name: String,
    pub status: RunStatus,
    pub started: Instant,
    /// Wall clock at start, ms since the epoch — for display only.
    pub started_ms: u64,
    pub settled: Option<Instant>,
    pub phases: Vec<PhaseSummary>,
    pub error: Option<String>,
}

impl RunSummary {
    pub fn new(id: &str, plan: &Plan) -> Self {
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        RunSummary {
            id: id.to_string(),
            name: plan.name.clone(),
            status: RunStatus::Running,
            started: Instant::now(),
            started_ms,
            settled: None,
            phases: plan
                .titles()
                .map(|t| PhaseSummary {
                    title: t.to_string(),
                    agents: Vec::new(),
                })
                .collect(),
            error: None,
        }
    }

    fn records(&self) -> impl Iterator<Item = &AgentRecord> {
        self.phases.iter().flat_map(|p| p.agents.iter())
    }

    pub fn tokens(&self) -> u64 {
        self.records().filter_map(|r| r.tokens).sum()
    }

    /// `(started, settled-normally, failed)` — what the cancel/summary lines quote.
    pub fn counts(&self) -> (usize, usize, usize) {
        let started = self.records().count();
        let finished = self
            .records()
            .filter(|r| matches!(r.status, AgentStatus::Done | AgentStatus::Failed))
            .count();
        let failed = self
            .records()
            .filter(|r| r.status == AgentStatus::Failed)
            .count();
        (started, finished, failed)
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.settled
            .unwrap_or_else(Instant::now)
            .duration_since(self.started)
            .as_millis() as u64
    }

    /// The `workflows_report` wire shape.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "status": self.status.as_str(),
            "started_ms": self.started_ms,
            "elapsed_ms": self.elapsed_ms(),
            "tokens": self.tokens(),
            "error": self.error,
            "phases": self.phases.iter().map(|p| json!({
                "title": p.title,
                "agents": p.agents.iter().map(|a| json!({
                    "id": a.id,
                    "label": a.label,
                    "status": a.status.as_str(),
                    "tokens": a.tokens,
                    "elapsed_ms": a.settled.unwrap_or_else(Instant::now).duration_since(a.started).as_millis() as u64,
                    "error": a.error,
                    "note": a.note,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
    }

    fn record(&mut self, phase: &str, rec: AgentRecord) {
        if let Some(p) = self.phases.iter_mut().find(|p| p.title == phase) {
            p.agents.push(rec);
        }
    }

    fn settle(&mut self, id: &str, status: AgentStatus, reply: &AgentReply) {
        if let Some(r) = self.records_mut().find(|r| r.id == id) {
            r.status = status;
            r.tokens = reply.tokens;
            r.settled = Some(Instant::now());
            r.error = reply.value.as_ref().err().cloned();
            r.note = reply.note.clone();
        }
    }

    fn records_mut(&mut self) -> impl Iterator<Item = &mut AgentRecord> {
        self.phases.iter_mut().flat_map(|p| p.agents.iter_mut())
    }
}

/// The outcome plus every finished phase's output, so a partial result can be
/// written even when the run errored.
#[derive(Debug)]
pub struct RunOutcome {
    pub result: Result<Value, RunError>,
    pub phases: Vec<(String, Value)>,
}

/// Finished phases and `args`.
pub struct Scope {
    pub args: Value,
    pub phases: Vec<(String, Value)>,
}

impl Lookup for Scope {
    fn get(&self, root: &str) -> Option<&Value> {
        if root == "args" {
            return Some(&self.args);
        }
        self.phases
            .iter()
            .rev()
            .find(|(t, _)| t == root)
            .map(|(_, v)| v)
    }
}

/// One extra root over a base scope: `item`/`prev`, or an `until_quiet`
/// phase's own union so far.
struct Overlay<'a> {
    base: &'a dyn Lookup,
    names: [(&'a str, Option<&'a Value>); 2],
}

impl Lookup for Overlay<'_> {
    fn get(&self, root: &str) -> Option<&Value> {
        for (name, value) in self.names {
            if name == root {
                return value;
            }
        }
        self.base.get(root)
    }
}

struct Exec<'a> {
    plan: &'a Plan,
    run_id: &'a str,
    runner: &'a dyn AgentRunner,
    obs: &'a dyn Observer,
    cancel: CancellationToken,
    gate: Arc<Semaphore>,
    local: Semaphore,
    cap: usize,
    started: AtomicUsize,
    summary: Arc<Mutex<RunSummary>>,
}

pub async fn run_plan(run: Run<'_>, runner: &dyn AgentRunner, obs: &dyn Observer) -> RunOutcome {
    let Run {
        plan,
        args,
        run_id,
        limits,
        gate,
        cancel,
        summary,
    } = run;
    let errors = plan.errors();
    if !errors.is_empty() {
        finish(&summary, RunStatus::Failed, Some("invalid plan".into()));
        return RunOutcome {
            result: Err(RunError::Invalid(errors)),
            phases: Vec::new(),
        };
    }
    let width = plan
        .concurrency
        .map_or(limits.concurrency, |c| c.min(limits.concurrency))
        .max(1);
    let cap = plan
        .max_agents
        .map_or(limits.max_agents, |m| m.min(limits.max_agents));
    let exec = Exec {
        plan,
        run_id: &run_id,
        runner,
        obs,
        cancel: cancel.clone(),
        gate,
        local: Semaphore::new(width),
        cap,
        started: AtomicUsize::new(0),
        summary: summary.clone(),
    };
    let mut scope = Scope {
        args,
        phases: Vec::new(),
    };
    let result = exec.drive(&mut scope).await;
    let result = match result {
        Err(RunError::Cancelled { .. }) => {
            let (started, finished, _) = lock(&summary).counts();
            finish(&summary, RunStatus::Cancelled, None);
            Err(RunError::Cancelled { finished, started })
        }
        Err(e) => {
            finish(&summary, RunStatus::Failed, Some(e.to_string()));
            Err(e)
        }
        Ok(v) => {
            finish(&summary, RunStatus::Done, None);
            Ok(v)
        }
    };
    RunOutcome {
        result,
        phases: scope.phases,
    }
}

fn lock(summary: &Mutex<RunSummary>) -> std::sync::MutexGuard<'_, RunSummary> {
    summary
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn finish(summary: &Mutex<RunSummary>, status: RunStatus, error: Option<String>) {
    let mut s = lock(summary);
    s.status = status;
    s.settled = Some(Instant::now());
    s.error = error;
}

/// Placeholder; `run_plan` fills the counts once every in-flight agent settled.
const CANCELLED: RunError = RunError::Cancelled {
    finished: 0,
    started: 0,
};

impl Exec<'_> {
    async fn drive(&self, scope: &mut Scope) -> Result<Value, RunError> {
        for phase in &self.plan.phases {
            if self.cancel.is_cancelled() {
                return Err(CANCELLED);
            }
            let value = match phase.shape().expect("validated") {
                Shape::Parallel(agents) => self.parallel(phase, agents, &*scope, "0").await?,
                Shape::Each { selector, stages } => {
                    self.each(phase, selector, stages, scope).await?
                }
                Shape::UntilQuiet { cfg, agents } => {
                    self.until_quiet(phase, cfg, agents, scope).await?
                }
            };
            scope.phases.push((phase.title.clone(), value));
        }
        match &self.plan.output {
            Some(sel) => Selector::parse(sel)
                .and_then(|s| s.eval(scope))
                .map_err(|source| RunError::Select {
                    phase: "output".into(),
                    source,
                }),
            None => Ok(scope
                .phases
                .last()
                .map(|(_, v)| v.clone())
                .unwrap_or(Value::Null)),
        }
    }

    /// Every agent at once; values in listed order, `null` for a failure.
    async fn parallel(
        &self,
        phase: &Phase,
        agents: &[AgentSpec],
        scope: &dyn Lookup,
        n: &str,
    ) -> Result<Value, RunError> {
        let values = join_all(agents.iter().map(|spec| self.agent(phase, spec, scope, n)))
            .await
            .into_iter()
            .collect::<Result<Vec<Value>, RunError>>()?;
        Ok(Value::Array(values))
    }

    /// Per-item pipelines, no barrier: item B can be in stage 2 while item A
    /// is still in stage 1. A `null` stage short-circuits the item's rest.
    async fn each(
        &self,
        phase: &Phase,
        selector: &str,
        stages: &[AgentSpec],
        scope: &Scope,
    ) -> Result<Value, RunError> {
        let select_err = |source| RunError::Select {
            phase: phase.title.clone(),
            source,
        };
        let sel = Selector::parse(selector).map_err(select_err)?;
        let items = match sel.eval(scope).map_err(select_err)? {
            Value::Array(items) => items,
            other => {
                return Err(select_err(SelectError::Eval {
                    selector: selector.to_string(),
                    message: format!(
                        "`each` needs an array, found {}",
                        crate::select::kind(&other)
                    ),
                }))
            }
        };
        if items.is_empty() {
            self.obs.note(&format!(
                "phase `{}`: `each = \"{selector}\"` selected nothing — skipped",
                phase.title
            ));
            return Ok(Value::Array(Vec::new()));
        }
        let pipelines = items.iter().enumerate().map(|(i, item)| async move {
            let mut prev: Option<Value> = None;
            for (si, stage) in stages.iter().enumerate() {
                let sc = Overlay {
                    base: scope,
                    names: [("item", Some(item)), ("prev", prev.as_ref())],
                };
                let n = if stages.len() == 1 {
                    i.to_string()
                } else {
                    format!("{i}.{si}")
                };
                let v = self.agent(phase, stage, &sc, &n).await?;
                if v.is_null() {
                    return Ok(Value::Null);
                }
                prev = Some(v);
            }
            Ok(prev.unwrap_or(Value::Null))
        });
        let values = join_all(pipelines)
            .await
            .into_iter()
            .collect::<Result<Vec<Value>, RunError>>()?;
        Ok(Value::Array(values))
    }

    /// Rounds of the phase's agents until `rounds` consecutive rounds add no
    /// new key, or `max_rounds`. The union so far is visible as `{{Title}}`.
    async fn until_quiet(
        &self,
        phase: &Phase,
        cfg: &UntilQuiet,
        agents: &[AgentSpec],
        scope: &Scope,
    ) -> Result<Value, RunError> {
        let keys: Vec<Vec<&str>> = cfg
            .key
            .split(',')
            .map(|k| k.trim().split('.').collect())
            .collect();
        let mut union: Vec<Value> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut quiet = 0;
        let mut rounds = 0;
        for round in 0..cfg.max_rounds {
            rounds = round + 1;
            let so_far = Value::Array(union.clone());
            let sc = Overlay {
                base: scope,
                names: [(&phase.title, Some(&so_far)), ("", None)],
            };
            let Value::Array(values) = self
                .parallel(phase, agents, &sc, &round.to_string())
                .await?
            else {
                unreachable!()
            };
            let mut added = 0;
            for element in values.into_iter().flat_map(flatten) {
                if seen.insert(key_of(&element, &keys)) {
                    union.push(element);
                    added += 1;
                }
            }
            if added == 0 {
                quiet += 1;
                if quiet >= cfg.rounds {
                    break;
                }
            } else {
                quiet = 0;
            }
        }
        self.obs.note(&format!(
            "phase `{}`: {} rounds, {} distinct elements",
            phase.title,
            rounds,
            union.len()
        ));
        Ok(Value::Array(union))
    }

    /// One spec: a single agent, or `votes` identical ones decided by majority.
    async fn agent(
        &self,
        phase: &Phase,
        spec: &AgentSpec,
        scope: &dyn Lookup,
        n: &str,
    ) -> Result<Value, RunError> {
        let Some(votes) = spec.votes.filter(|v| *v > 1) else {
            return self.dispatch(phase, spec, scope, n).await;
        };
        let replies = join_all((0..votes).map(|v| {
            let n = format!("{n}.{v}");
            async move { self.dispatch(phase, spec, scope, &n).await }
        }))
        .await
        .into_iter()
        .collect::<Result<Vec<Value>, RunError>>()?;
        let accept = spec.accept.as_deref().unwrap_or_default();
        let path: Vec<&str> = accept.split('.').collect();
        let yes = replies
            .iter()
            .filter(|v| truthy(field(v, &path).unwrap_or(&Value::Null)))
            .count();
        Ok(json!({ "accepted": yes * 2 > votes, "votes": replies }))
    }

    /// Start one agent: render, count against the cap, take both permits,
    /// run, record. `null` for any failure — the run continues.
    async fn dispatch(
        &self,
        phase: &Phase,
        spec: &AgentSpec,
        scope: &dyn Lookup,
        n: &str,
    ) -> Result<Value, RunError> {
        let render = |text: &str| {
            Template::parse(text)
                .and_then(|t| t.render(scope))
                .map_err(|source| RunError::Template {
                    phase: phase.title.clone(),
                    label: spec.label.clone(),
                    source,
                })
        };
        let label = render(&spec.label)?;
        let prompt = render(&spec.prompt)?;
        if self.cancel.is_cancelled() {
            return Err(CANCELLED);
        }
        if self.started.fetch_add(1, Ordering::SeqCst) >= self.cap {
            return Err(RunError::AgentCap {
                phase: phase.title.clone(),
                cap: self.cap,
            });
        }
        // Run width first, then the process gate: a queued run never holds a
        // gate slot it cannot use yet.
        let _local = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(CANCELLED),
            p = self.local.acquire() => p.expect("never closed"),
        };
        let _gate = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(CANCELLED),
            p = self.gate.acquire() => p.expect("never closed"),
        };
        let id = format!("{}:{}:{label}:{n}", self.run_id, phase.title);
        let req = AgentRequest {
            id: id.clone(),
            phase: phase.title.clone(),
            label: label.clone(),
            prompt,
            schema: spec.schema.clone(),
            agent: spec.agent.clone(),
            model: spec.model.clone(),
            effort: spec.effort,
            isolation: spec.isolation,
            max_turns: spec.max_turns,
        };
        lock(&self.summary).record(
            &phase.title,
            AgentRecord {
                id: id.clone(),
                label: label.clone(),
                status: AgentStatus::Running,
                tokens: None,
                started: Instant::now(),
                settled: None,
                error: None,
                note: None,
            },
        );
        self.obs.started(&id, &phase.title, &label);
        let reply = self.runner.run(req, self.cancel.child_token()).await;
        let status = match (&reply.value, self.cancel.is_cancelled()) {
            (Ok(_), _) => AgentStatus::Done,
            (Err(_), true) => AgentStatus::Cancelled,
            (Err(_), false) => AgentStatus::Failed,
        };
        lock(&self.summary).settle(&id, status, &reply);
        self.obs.finished(&id, reply.value.is_ok(), reply.tokens);
        if self.cancel.is_cancelled() {
            return Err(CANCELLED);
        }
        Ok(reply.value.unwrap_or(Value::Null))
    }
}

/// An `until_quiet` value's elements: an array spreads (nulls dropped), a
/// `null` agent contributes nothing, anything else is one element.
fn flatten(v: Value) -> Vec<Value> {
    match v {
        Value::Null => Vec::new(),
        Value::Array(items) => items.into_iter().filter(|i| !i.is_null()).collect(),
        other => vec![other],
    }
}

fn field<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(
        v,
        |cur, k| if k.is_empty() { Some(cur) } else { cur.get(*k) },
    )
}

/// Dedup key: each field path's compact JSON, joined; a missing field is `null`.
fn key_of(element: &Value, keys: &[Vec<&str>]) -> String {
    keys.iter()
        .map(|path| field(element, path).unwrap_or(&Value::Null).to_string())
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

/// JS truthiness: `false`, `null`, `0`, `""` are false; arrays and objects are true.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    type Script = Box<dyn Fn(&AgentRequest) -> Result<Value, String> + Send + Sync>;

    /// Scripted runner: a closure answers by request, every answer costs 100
    /// tokens, and the high-water mark of concurrent runs is recorded.
    pub(crate) struct FakeRunner {
        script: Script,
        pub seen: Mutex<Vec<AgentRequest>>,
        running: AtomicUsize,
        pub max_seen: AtomicUsize,
        delay: Duration,
        /// Per-label order of arrival, for pipelining assertions.
        pub order: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        pub(crate) fn new(
            script: impl Fn(&AgentRequest) -> Result<Value, String> + Send + Sync + 'static,
        ) -> Self {
            FakeRunner {
                script: Box::new(script),
                seen: Mutex::new(Vec::new()),
                running: AtomicUsize::new(0),
                max_seen: AtomicUsize::new(0),
                delay: Duration::ZERO,
                order: Mutex::new(Vec::new()),
            }
        }
        pub(crate) fn delay(mut self, d: Duration) -> Self {
            self.delay = d;
            self
        }
        pub(crate) fn labels(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.label.clone())
                .collect()
        }
    }

    impl AgentRunner for FakeRunner {
        fn run(&self, req: AgentRequest, cancel: CancellationToken) -> BoxFuture<'_, AgentReply> {
            Box::pin(async move {
                let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_seen.fetch_max(now, Ordering::SeqCst);
                self.order.lock().unwrap().push(req.label.clone());
                let value = (self.script)(&req);
                self.seen.lock().unwrap().push(req);
                let value = tokio::select! {
                    _ = cancel.cancelled() => Err("cancelled".to_string()),
                    _ = tokio::time::sleep(self.delay) => value,
                };
                self.running.fetch_sub(1, Ordering::SeqCst);
                AgentReply {
                    value,
                    tokens: Some(100),
                    note: None,
                }
            })
        }
    }

    /// Records every observer call.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub events: Mutex<Vec<String>>,
    }

    impl Observer for Recorder {
        fn started(&self, id: &str, phase: &str, label: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("start {id} [{phase}/{label}]"));
        }
        fn finished(&self, id: &str, ok: bool, tokens: Option<u64>) {
            self.events
                .lock()
                .unwrap()
                .push(format!("done {id} ok={ok} tok={tokens:?}"));
        }
        fn note(&self, text: &str) {
            self.events.lock().unwrap().push(format!("note {text}"));
        }
    }

    pub(crate) fn plan(json: Value) -> Plan {
        let p = Plan::from_json(json).unwrap();
        assert!(p.errors().is_empty(), "{:?}", p.errors());
        p
    }

    pub(crate) struct Harness {
        pub limits: Limits,
        pub gate: Arc<Semaphore>,
        pub cancel: CancellationToken,
        pub args: Value,
    }

    impl Default for Harness {
        fn default() -> Self {
            Harness {
                limits: Limits {
                    concurrency: 8,
                    max_agents: 1000,
                },
                gate: Arc::new(Semaphore::new(8)),
                cancel: CancellationToken::new(),
                args: json!({}),
            }
        }
    }

    impl Harness {
        pub(crate) async fn run(
            &self,
            plan: &Plan,
            runner: &dyn AgentRunner,
            obs: &dyn Observer,
        ) -> (RunOutcome, Arc<Mutex<RunSummary>>) {
            let summary = Arc::new(Mutex::new(RunSummary::new("r1", plan)));
            let out = run_plan(
                Run {
                    plan,
                    args: self.args.clone(),
                    run_id: "r1".into(),
                    limits: self.limits,
                    gate: self.gate.clone(),
                    cancel: self.cancel.clone(),
                    summary: summary.clone(),
                },
                runner,
                obs,
            )
            .await;
            (out, summary)
        }
    }

    fn by_label(req: &AgentRequest) -> Result<Value, String> {
        match req.label.as_str() {
            "fail" => Err("refused".into()),
            l => Ok(json!({"label": l, "prompt": req.prompt})),
        }
    }

    #[tokio::test]
    async fn parallel_values_keep_listed_order_with_null_for_a_failure() {
        let plan = plan(json!({"name": "p", "phases": [{"title": "A", "agents": [
            {"label": "one", "prompt": "p1 {{args.x}}"},
            {"label": "fail", "prompt": "p2"},
            {"label": "two", "prompt": "p3"}
        ]}]}));
        let runner = FakeRunner::new(by_label).delay(Duration::from_millis(5));
        let obs = Recorder::default();
        let h = Harness {
            args: json!({"x": 42}),
            ..Default::default()
        };
        let (out, summary) = h.run(&plan, &runner, &obs).await;
        let v = out.result.unwrap();
        assert_eq!(v[0]["prompt"], "p1 42");
        assert!(v[1].is_null());
        assert_eq!(v[2]["label"], "two");
        assert_eq!(out.phases, vec![("A".to_string(), v)]);
        let s = lock(&summary);
        assert_eq!(s.status, RunStatus::Done);
        assert_eq!(s.counts(), (3, 3, 1));
        assert_eq!(s.tokens(), 300);
        assert!(s.settled.is_some());
        let rec = &s.phases[0].agents[1];
        assert_eq!(
            (rec.status, rec.error.as_deref()),
            (AgentStatus::Failed, Some("refused"))
        );
        let events = obs.events.lock().unwrap().clone();
        let ids: HashSet<&str> = events
            .iter()
            .filter_map(|e| e.strip_prefix("start "))
            .map(|e| e.split(' ').next().unwrap())
            .collect();
        assert_eq!(ids.len(), 3, "distinct ids: {events:?}");
        assert!(ids.contains("r1:A:one:0"));
        assert!(
            events
                .iter()
                .any(|e| e == "done r1:A:fail:0 ok=false tok=Some(100)"),
            "{events:?}"
        );
        assert_eq!(runner.max_seen.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn the_run_width_is_the_smaller_of_plan_and_gate() {
        let six = json!({"name": "p", "concurrency": 2, "phases": [{"title": "A", "agents": (0..6).map(|i| json!({"label": format!("a{i}"), "prompt": "p"})).collect::<Vec<_>>()}]});
        let plan = plan(six);
        let runner = FakeRunner::new(by_label).delay(Duration::from_millis(20));
        let h = Harness::default();
        h.run(&plan, &runner, &Silent).await.0.result.unwrap();
        assert_eq!(
            runner.max_seen.load(Ordering::SeqCst),
            2,
            "plan.concurrency lowers the width"
        );

        // The gate wins when the plan asks for more than it allows.
        let mut wide = plan.clone();
        wide.concurrency = Some(100);
        let runner = FakeRunner::new(by_label).delay(Duration::from_millis(20));
        let h = Harness {
            limits: Limits {
                concurrency: 3,
                max_agents: 1000,
            },
            gate: Arc::new(Semaphore::new(3)),
            ..Default::default()
        };
        h.run(&wide, &runner, &Silent).await.0.result.unwrap();
        assert_eq!(
            runner.max_seen.load(Ordering::SeqCst),
            3,
            "a plan can never raise the width"
        );
    }

    #[tokio::test]
    async fn max_agents_is_a_run_error_never_a_silent_truncation() {
        let plan = plan(
            json!({"name": "p", "max_agents": 2, "phases": [{"title": "A", "agents": [
                {"label": "a", "prompt": "p"}, {"label": "b", "prompt": "p"}, {"label": "c", "prompt": "p"}
            ]}]}),
        );
        let runner = FakeRunner::new(by_label);
        let (out, summary) = Harness::default().run(&plan, &runner, &Silent).await;
        let err = out.result.unwrap_err();
        assert_eq!(
            err,
            RunError::AgentCap {
                phase: "A".into(),
                cap: 2
            }
        );
        assert_eq!(lock(&summary).status, RunStatus::Failed);
        assert!(lock(&summary)
            .error
            .as_deref()
            .unwrap()
            .contains("max_agents"));

        // During growth: an until_quiet phase that never goes quiet hits the cap live.
        let plan = self::plan(
            json!({"name": "p", "max_agents": 3, "phases": [{"title": "F", "until_quiet": {"rounds": 2, "max_rounds": 50, "key": "n"}, "agents": [{"label": "f", "prompt": "{{F}}"}]}]}),
        );
        let counter = AtomicUsize::new(0);
        let runner =
            FakeRunner::new(move |_| Ok(json!([{"n": counter.fetch_add(1, Ordering::SeqCst)}])));
        let (out, _) = Harness::default().run(&plan, &runner, &Silent).await;
        assert_eq!(
            out.result.unwrap_err(),
            RunError::AgentCap {
                phase: "F".into(),
                cap: 3
            }
        );
        assert_eq!(runner.labels().len(), 3);
    }

    #[tokio::test]
    async fn cancel_stops_scheduling_and_interrupts_in_flight_agents() {
        let plan = plan(json!({"name": "p", "phases": [
            {"title": "A", "agents": [{"label": "slow1", "prompt": "p"}, {"label": "slow2", "prompt": "p"}]},
            {"title": "B", "agents": [{"label": "never", "prompt": "p"}]}
        ]}));
        let runner = FakeRunner::new(by_label).delay(Duration::from_secs(30));
        let h = Harness::default();
        let cancel = h.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let started = Instant::now();
        let (out, summary) = h.run(&plan, &runner, &Silent).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "in-flight agents were not interrupted"
        );
        assert_eq!(
            out.result.unwrap_err(),
            RunError::Cancelled {
                finished: 0,
                started: 2
            }
        );
        assert_eq!(
            runner.labels(),
            ["slow1", "slow2"],
            "phase B never scheduled"
        );
        let s = lock(&summary);
        assert_eq!(s.status, RunStatus::Cancelled);
        assert!(s.phases[0]
            .agents
            .iter()
            .all(|a| a.status == AgentStatus::Cancelled));
        assert!(out.phases.is_empty());
    }

    #[tokio::test]
    async fn each_pipelines_items_without_a_barrier_and_short_circuits_on_null() {
        let plan = plan(json!({"name": "p", "phases": [
            {"title": "Find", "agents": [{"label": "lister", "prompt": "list"}]},
            {"title": "Check", "each": "Find[0].items[*]", "stages": [
                {"label": "s1:{{item.file}}", "prompt": "first {{item.file}}"},
                {"label": "s2:{{item.file}}", "prompt": "second {{item.file}} after {{prev.stage}}"}
            ]}
        ]}));
        let runner = FakeRunner::new(|req: &AgentRequest| match req.label.as_str() {
            "lister" => Ok(json!({"items": [{"file": "a"}, {"file": "b"}, {"file": "bad"}]})),
            "s1:bad" => Err("refused".into()),
            l => Ok(json!({"stage": l, "prompt": req.prompt})),
        });
        let runner = runner.delay(Duration::from_millis(10));
        let obs = Recorder::default();
        let (out, _) = Harness::default().run(&plan, &runner, &obs).await;
        let v = out.result.unwrap();
        assert_eq!(v[0]["stage"], "s2:a");
        assert_eq!(v[0]["prompt"], "second a after s1:a");
        assert_eq!(v[1]["stage"], "s2:b");
        assert!(v[2].is_null(), "a null stage short-circuits the item: {v}");
        let labels = runner.labels();
        assert!(
            !labels.contains(&"s2:bad".to_string()),
            "stage 2 must not run after a null: {labels:?}"
        );
        // No-barrier pipelining is pinned by `each_has_no_barrier_between_stages`;
        // here the ids must at least be distinct per item and stage.
        let events = obs.events.lock().unwrap().clone();
        let starts: Vec<&str> = events
            .iter()
            .filter_map(|e| e.strip_prefix("start "))
            .collect();
        assert!(
            starts.iter().any(|s| s.starts_with("r1:Check:s1:a:0.0 ")),
            "{starts:?}"
        );
        assert!(
            starts.iter().any(|s| s.starts_with("r1:Check:s2:b:1.1 ")),
            "{starts:?}"
        );
    }

    #[tokio::test]
    async fn each_has_no_barrier_between_stages() {
        // Width 2. Item a's stage 1 blocks until it sees item b reach stage 2
        // — only possible if b is not waiting on a barrier behind a.
        let plan = plan(json!({"name": "p", "phases": [
            {"title": "Check", "each": "args.items", "stages": [
                {"label": "s1:{{item}}", "prompt": "p"},
                {"label": "s2:{{item}}", "prompt": "p"}
            ]}
        ]}));
        struct Gate(std::sync::Arc<tokio::sync::Notify>, AtomicUsize);
        impl AgentRunner for Gate {
            fn run(
                &self,
                req: AgentRequest,
                _cancel: CancellationToken,
            ) -> BoxFuture<'_, AgentReply> {
                Box::pin(async move {
                    if req.label == "s1:a" {
                        tokio::time::timeout(Duration::from_secs(5), self.0.notified())
                            .await
                            .expect("item b never reached stage 2: stages are barriered");
                    }
                    if req.label == "s2:b" {
                        self.0.notify_one();
                    }
                    self.1.fetch_add(1, Ordering::SeqCst);
                    AgentReply {
                        value: Ok(json!(req.label)),
                        tokens: None,
                        note: None,
                    }
                })
            }
        }
        let runner = Gate(Arc::new(tokio::sync::Notify::new()), AtomicUsize::new(0));
        let h = Harness {
            args: json!({"items": ["a", "b"]}),
            ..Default::default()
        };
        let (out, _) = h.run(&plan, &runner, &Silent).await;
        assert_eq!(out.result.unwrap(), json!(["s2:a", "s2:b"]));
        assert_eq!(runner.1.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn an_empty_each_selection_is_a_noted_noop() {
        let plan = plan(json!({"name": "p", "phases": [
            {"title": "Check", "each": "args.items", "stages": [{"label": "s", "prompt": "{{item}}"}]}
        ]}));
        let runner = FakeRunner::new(by_label);
        let obs = Recorder::default();
        let h = Harness {
            args: json!({"items": []}),
            ..Default::default()
        };
        let (out, _) = h.run(&plan, &runner, &obs).await;
        assert_eq!(out.result.unwrap(), json!([]));
        assert!(runner.labels().is_empty());
        let events = obs.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| e.contains("selected nothing")),
            "{events:?}"
        );

        // A selector that misses is a run error naming the phase and path.
        let h = Harness {
            args: json!({"nope": 1}),
            ..Default::default()
        };
        let (out, summary) = h.run(&plan, &runner, &obs).await;
        let err = out.result.unwrap_err().to_string();
        assert!(
            err.starts_with("phase `Check`: selector `args.items`"),
            "{err}"
        );
        assert_eq!(lock(&summary).status, RunStatus::Failed);
        let h = Harness {
            args: json!({"items": "not an array"}),
            ..Default::default()
        };
        let err = h
            .run(&plan, &runner, &obs)
            .await
            .0
            .result
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`each` needs an array, found a string"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn votes_decide_by_majority_and_a_null_vote_is_a_no() {
        let plan = plan(json!({"name": "p", "phases": [{"title": "V", "agents": [
            {"label": "yes", "prompt": "p", "votes": 3, "accept": "verdict.real", "schema": {"type": "object"}},
            {"label": "tie", "prompt": "p", "votes": 3, "accept": "real", "schema": {"type": "object"}}
        ]}]}));
        let n = AtomicUsize::new(0);
        let runner = FakeRunner::new(move |req: &AgentRequest| {
            let i = n.fetch_add(1, Ordering::SeqCst);
            match req.label.as_str() {
                // 2 of 3 yes → accepted (the third is a failure → null → no).
                "yes" if req.id.ends_with(".2") => Err("refused".into()),
                "yes" => Ok(json!({"verdict": {"real": true}})),
                // 1 yes, 1 no, 1 null → not accepted.
                "tie" if req.id.ends_with(".0") => Ok(json!({"real": 1})),
                "tie" if req.id.ends_with(".1") => Ok(json!({"real": ""})),
                _ => Err(format!("refused {i}")),
            }
        });
        let (out, summary) = Harness::default().run(&plan, &runner, &Silent).await;
        let v = out.result.unwrap();
        assert_eq!(v[0]["accepted"], true);
        assert_eq!(v[0]["votes"].as_array().unwrap().len(), 3);
        assert!(v[0]["votes"][2].is_null());
        assert_eq!(v[1]["accepted"], false);
        let ids: Vec<String> = lock(&summary).phases[0]
            .agents
            .iter()
            .map(|a| a.id.clone())
            .collect();
        assert_eq!(ids.len(), 6);
        assert!(
            ids.contains(&"r1:V:yes:0.0".to_string()) && ids.contains(&"r1:V:tie:0.2".to_string()),
            "{ids:?}"
        );
    }

    #[tokio::test]
    async fn until_quiet_stops_after_quiet_rounds_dedups_by_composite_key_and_sees_the_union() {
        let plan = plan(
            json!({"name": "p", "phases": [{"title": "Find", "until_quiet": {"rounds": 2, "max_rounds": 10, "key": "file,line"}, "agents": [
                {"label": "f1", "prompt": "so far: {{Find}}"},
                {"label": "f2", "prompt": "so far: {{Find}}"}
            ]}]}),
        );
        let round = Arc::new(AtomicUsize::new(0));
        let seen_round = round.clone();
        let runner = FakeRunner::new(move |req: &AgentRequest| {
            let r: usize = req.id.rsplit(':').next().unwrap().parse().unwrap();
            seen_round.fetch_max(r, Ordering::SeqCst);
            Ok(match (r, req.label.as_str()) {
                (0, "f1") => json!([{"file": "a", "line": 1}, {"file": "a", "line": 2}]),
                (0, "f2") => json!([{"file": "a", "line": 1, "extra": "dup"}]),
                (1, "f1") => json!([{"file": "b", "line": 1}]),
                (1, "f2") => json!({"file": "a", "line": 2, "non-array": "one element"}),
                // Rounds 2 and 3 add nothing → quiet twice → stop at 4 rounds.
                _ => json!([{"file": "a", "line": 1}]),
            })
        });
        let obs = Recorder::default();
        let (out, _) = Harness::default().run(&plan, &runner, &obs).await;
        let v = out.result.unwrap();
        assert_eq!(
            v,
            json!([{"file": "a", "line": 1}, {"file": "a", "line": 2}, {"file": "b", "line": 1}]),
            "first-seen order, dedup by key"
        );
        assert_eq!(round.load(Ordering::SeqCst), 3, "rounds 0..=3 ran");
        let seen = runner.seen.lock().unwrap();
        let r1 = seen.iter().find(|r| r.id == "r1:Find:f1:1").unwrap();
        assert_eq!(
            r1.prompt,
            r#"so far: [{"file":"a","line":1},{"file":"a","line":2}]"#
        );
        let events = obs.events.lock().unwrap().clone();
        assert!(
            events
                .iter()
                .any(|e| e == "note phase `Find`: 4 rounds, 3 distinct elements"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn until_quiet_respects_max_rounds() {
        let plan = plan(
            json!({"name": "p", "phases": [{"title": "Find", "until_quiet": {"rounds": 2, "max_rounds": 3, "key": "n"}, "agents": [{"label": "f", "prompt": "p"}]}]}),
        );
        let n = AtomicUsize::new(0);
        let runner = FakeRunner::new(move |_| Ok(json!([{"n": n.fetch_add(1, Ordering::SeqCst)}])));
        let (out, _) = Harness::default().run(&plan, &runner, &Silent).await;
        assert_eq!(out.result.unwrap(), json!([{"n": 0}, {"n": 1}, {"n": 2}]));
        assert_eq!(runner.labels().len(), 3);
    }

    #[tokio::test]
    async fn output_defaults_to_the_last_phase_or_follows_the_selector() {
        let two = json!([
            {"title": "A", "agents": [{"label": "a", "prompt": "p"}]},
            {"title": "B", "agents": [{"label": "b", "prompt": "{{A[0].label}}"}]}
        ]);
        let plan = plan(json!({"name": "p", "phases": two}));
        let runner = FakeRunner::new(by_label);
        let (out, _) = Harness::default().run(&plan, &runner, &Silent).await;
        let v = out.result.unwrap();
        assert_eq!(v[0]["label"], "b");
        assert_eq!(v[0]["prompt"], "a");

        let plan = self::plan(json!({"name": "p", "output": "A[0].label", "phases": two}));
        let (out, _) = Harness::default().run(&plan, &runner, &Silent).await;
        assert_eq!(out.result.unwrap(), json!("a"));
        assert_eq!(out.phases.len(), 2);
    }

    #[tokio::test]
    async fn an_invalid_plan_runs_nothing() {
        let plan = Plan::from_json(json!({"name": "Bad", "phases": []})).unwrap();
        let runner = FakeRunner::new(by_label);
        let (out, summary) = Harness::default().run(&plan, &runner, &Silent).await;
        assert!(matches!(out.result, Err(RunError::Invalid(ref e)) if e.len() == 2));
        assert!(runner.labels().is_empty());
        assert_eq!(lock(&summary).status, RunStatus::Failed);
    }

    #[test]
    fn summary_json_has_the_wire_shape() {
        let plan = plan(
            json!({"name": "p", "phases": [{"title": "A", "agents": [{"label": "a", "prompt": "p"}]}]}),
        );
        let mut s = RunSummary::new("r9", &plan);
        s.record(
            "A",
            AgentRecord {
                id: "r9:A:a:0".into(),
                label: "a".into(),
                status: AgentStatus::Running,
                tokens: None,
                started: Instant::now(),
                settled: None,
                error: None,
                note: None,
            },
        );
        s.settle(
            "r9:A:a:0",
            AgentStatus::Done,
            &AgentReply {
                value: Ok(json!(1)),
                tokens: Some(1200),
                note: Some("kept".into()),
            },
        );
        let j = s.to_json();
        assert_eq!(j["id"], "r9");
        assert_eq!(j["name"], "p");
        assert_eq!(j["status"], "running");
        assert_eq!(j["tokens"], 1200);
        assert_eq!(j["phases"][0]["title"], "A");
        assert_eq!(j["phases"][0]["agents"][0]["label"], "a");
        assert_eq!(j["phases"][0]["agents"][0]["status"], "done");
        assert_eq!(j["phases"][0]["agents"][0]["tokens"], 1200);
        assert_eq!(j["phases"][0]["agents"][0]["note"], "kept");
        assert!(j["phases"][0]["agents"][0]["elapsed_ms"].is_u64());
    }
}
