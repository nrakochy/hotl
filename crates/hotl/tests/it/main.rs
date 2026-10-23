//! Every hotl integration test but `tmux_smoke` and `update_offline`, one
//! module per former test binary. Those two keep their own binaries: nextest
//! schedules them specially by binary name (`.config/nextest.toml`).
//!
//! `crates/hotl` has no lib target, so the modules the tests need are pulled
//! in by path, once, at the root: `acp.rs` reaches `crate::wire` and
//! `crate::images` the same way it does in the binary.

#[path = "../../src/acp.rs"]
#[allow(dead_code)]
mod acp;
#[cfg(unix)]
#[path = "../../src/acp_child.rs"]
#[allow(dead_code)]
mod acp_child;
#[path = "../../src/features.rs"]
#[allow(dead_code)]
mod features;
#[path = "../../src/images.rs"]
#[allow(dead_code)]
mod images;
#[path = "../../src/schema_cli.rs"]
#[allow(dead_code)]
mod schema_cli;
#[path = "../../src/wire.rs"]
#[allow(dead_code)]
mod wire;

mod acp_protocol;
mod acp_subagent;
mod agent_plugins_conformance;
mod json_stream_schema;
mod tui_e2e;
mod watch_sees_tui_state;
mod wire_schema;
