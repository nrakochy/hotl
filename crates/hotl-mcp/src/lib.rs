//! M3a — MCP client (system-design §L4; sub-plan 0003; SECURITY.md §M3a).
//!
//! Shape: one `mcp` meta-tool covers every configured server (deferred
//! loading — the tool block stays small and cache-stable; schemas enter
//! context on first use). Every string a server returns passes exactly one
//! sanitizer chokepoint before reaching the transcript, and the first use of
//! each server raises a protected ask carrying the binary's hash.

pub mod client;
pub mod config;
pub mod sanitize;
pub mod tool;
pub mod trust;

pub use tool::McpTool;
