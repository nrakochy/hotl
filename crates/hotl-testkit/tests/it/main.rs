//! Every hotl-testkit integration test but `loop_overhead`, one module per
//! former test binary. `loop_overhead` keeps its own binary: it is a timing
//! gate that wants the machine to itself.

mod admission_speculation;
mod aside;
mod background_jobs;
mod child_notify;
mod clear_cache;
mod fold_survival;
mod fork_cache;
mod minified_read;
mod openai_breakpoints;
mod plan_toggle_cache;
mod predictions;
mod todo_reminder_cache;
