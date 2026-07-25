//! M3a — MCP client (SECURITY.md §M3a).
//!
//! Shape: one `mcp` meta-tool covers every configured server (deferred
//! loading — the tool block stays small and cache-stable; schemas enter
//! context on first use).
//!
//! INVARIANT: every string a server returns passes exactly one sanitizer
//! chokepoint ([`sanitize::wrap`]) before reaching the transcript — and it is
//! literally one, shared with `hotl-retrieval` rather than copied. Enforced by
//! `forged_attributes_in_the_source_cannot_escape`, `category_cf_is_stripped`,
//! `defang_is_two_sided`, and `there_is_exactly_one_sanitizer_in_this_crate`.
//!
//! INVARIANT: no server is spawned, and no trust grant is persisted, without a
//! `permission()` screen of the same [`trust::Fingerprint`] immediately before
//! — and that fingerprint covers `(command, args, binary hash)`, so repointing
//! a server name at a different package re-raises the protected ask. Enforced
//! by `run_without_a_permission_screen_records_no_grant_and_refuses`,
//! `a_binary_swapped_after_the_screen_does_not_connect`, and
//! `args_are_part_of_the_trust_key`.
//!
//! INVARIANT: no call waits on a connection that cannot answer — reads and
//! writes are bounded, a dead client is replaced rather than waited on, and the
//! user's cancellation token is honoured. Enforced by
//! `an_unbounded_line_is_bounded`, `a_blocked_writer_does_not_hang_forever`,
//! `a_crashed_server_is_reconnected_not_waited_on`, and
//! `escape_cancels_an_in_flight_call`.

pub mod client;
pub mod config;
pub mod sanitize;
pub mod tool;
pub mod trust;

pub use tool::{Connector, McpTool};
