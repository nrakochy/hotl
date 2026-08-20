//! hotl's declarative multi-agent workflow runner (0044): a [`Plan`] of phases
//! — parallel, per-item `each` pipelines, `until_quiet` loops — executed with
//! bounded concurrency by an `AgentRunner` the host supplies. Pure: no
//! engine or tool deps, so the executor is tested with a fake runner.

pub mod plan;
pub mod select;
pub mod structured;
pub mod summary;
pub mod template;

pub use plan::{AgentSpec, Effort, Isolation, Phase, Plan, PlanError, Severity, Shape};
pub use summary::Estimate;
