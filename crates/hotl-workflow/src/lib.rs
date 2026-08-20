//! hotl's declarative multi-agent workflow runner (0044): a [`Plan`] of phases
//! — parallel, per-item `each` pipelines, `until_quiet` loops — executed with
//! bounded concurrency by an `AgentRunner` the host supplies. Pure: no
//! engine or tool deps, so the executor is tested with a fake runner.

pub mod discover;
pub mod exec;
pub mod mermaid;
pub mod plan;
pub mod select;
pub mod structured;
pub mod summary;
pub mod template;

pub use discover::{discover, Found};
pub use exec::{
    run_plan, AgentReply, AgentRequest, AgentRunner, AgentStatus, Limits, Observer, Run, RunError,
    RunOutcome, RunStatus, RunSummary, Silent,
};
pub use plan::{
    json_schema, AgentSpec, Effort, Isolation, Phase, Plan, PlanError, Severity, Shape,
};
pub use summary::Estimate;
