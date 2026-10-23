//! `hotl doctor --args` (0075 T1, D8/D12): how often would schema-directed
//! argument coercion have changed a builtin tool call? Measured offline from
//! the session logs that already hold every `tool_use` input — no new
//! logging. Replays each recent session, runs `hotl_tools::coerce::mismatches`
//! against the builtin schemas, and counts the Parse-fatal turn ends the
//! json-repair could not save. The number the owner reads before T5 exists.

use std::collections::BTreeMap;
use std::path::Path;

use hotl_tools::coerce::{mismatches, Mismatch};
use hotl_tools::Registry;
use hotl_types::{assistant_tool_uses, Item};

/// Sessions whose header model names a test double, not a provider.
pub const SKIP_MODELS: [&str; 4] = ["m", "test-model", "claude-smoke", "fake"];

/// The text the Anthropic parser puts in a `Cancelled` reason when a tool
/// body did not parse (`sse.rs::on_block_stop`).
const PARSE_FATAL: &str = "tool input didn't parse";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub sessions: usize,
    pub skipped_sessions: usize,
    pub builtin_calls: u64,
    pub other_calls: u64,
    /// `tool → class → count`.
    pub by_tool: BTreeMap<String, BTreeMap<&'static str, u64>>,
    /// Calls with at least one mismatch.
    pub calls_touched: u64,
    pub parse_fatal: u64,
    pub oldest_ms: Option<u64>,
    pub newest_ms: Option<u64>,
}

impl Report {
    pub fn touched_pct(&self) -> f64 {
        if self.builtin_calls == 0 {
            0.0
        } else {
            self.calls_touched as f64 * 100.0 / self.builtin_calls as f64
        }
    }

    /// The plain table `hotl doctor --args` prints.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "sessions: {} replayed, {} skipped (test models / too old)\n",
            self.sessions, self.skipped_sessions
        ));
        if let (Some(a), Some(b)) = (self.oldest_ms, self.newest_ms) {
            out.push_str(&format!("range: {} .. {} (unix ms)\n", a, b));
        }
        out.push_str(&format!(
            "builtin tool calls: {}   other (MCP/plugin) calls skipped: {}\n",
            self.builtin_calls, self.other_calls
        ));
        out.push_str(&format!(
            "calls coercion would touch: {} ({:.2}% of builtin calls)\n",
            self.calls_touched,
            self.touched_pct()
        ));
        out.push_str(&format!(
            "parse-fatal turn ends (json-repair could not save the body): {}\n",
            self.parse_fatal
        ));
        if self.by_tool.is_empty() {
            out.push_str("no mismatches\n");
        } else {
            out.push_str("\ntool              class               count\n");
            for (tool, classes) in &self.by_tool {
                for (class, n) in classes {
                    out.push_str(&format!("{tool:<17} {class:<19} {n}\n"));
                }
            }
        }
        out
    }
}

/// Replay every session under `sessions_dir` newer than `days` and count.
pub fn run(sessions_dir: &Path, days: u64, now_ms: u64) -> Report {
    let defs: BTreeMap<String, serde_json::Value> = Registry::builtin()
        .defs()
        .into_iter()
        .map(|d| (d.name, d.input_schema))
        .collect();
    let cutoff = now_ms.saturating_sub(days * 24 * 3600 * 1000);
    let mut report = Report::default();
    for (_, path, _) in hotl_store::list_sessions(sessions_dir) {
        let Ok(replayed) = hotl_store::replay(&path) else {
            continue;
        };
        let header = &replayed.header;
        if header.created_at_ms < cutoff || SKIP_MODELS.contains(&header.model.as_str()) {
            report.skipped_sessions += 1;
            continue;
        }
        report.sessions += 1;
        report.oldest_ms = Some(
            report
                .oldest_ms
                .map_or(header.created_at_ms, |o| o.min(header.created_at_ms)),
        );
        report.newest_ms = Some(
            report
                .newest_ms
                .map_or(header.created_at_ms, |n| n.max(header.created_at_ms)),
        );
        for item in &replayed.items {
            let Item::Assistant { blocks, .. } = item else {
                continue;
            };
            for tu in assistant_tool_uses(blocks) {
                let Some(schema) = defs.get(&tu.name) else {
                    report.other_calls += 1;
                    continue;
                };
                report.builtin_calls += 1;
                let found = mismatches(schema, &tu.input);
                if !found.is_empty() {
                    report.calls_touched += 1;
                }
                for m in found {
                    *report
                        .by_tool
                        .entry(tu.name.clone())
                        .or_default()
                        .entry(class_of(&m))
                        .or_insert(0) += 1;
                }
            }
        }
        report.parse_fatal += count_parse_fatal(&path);
    }
    report
}

fn class_of(m: &Mismatch) -> &'static str {
    m.class()
}

/// `Cancelled` entries whose reason names a tool body the repair could not
/// save. A line scan: the projection drops cancellations, and this is the one
/// reader that wants them.
fn count_parse_fatal(path: &Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    text.lines()
        .filter(|l| l.contains("\"cancelled\"") && l.contains(PARSE_FATAL))
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_store::{Masker, SessionLog};
    use hotl_types::{EntryPayload, ToolResultItem};
    use serde_json::json;

    fn session_with(
        dir: &Path,
        model: &str,
        created_ms: u64,
        inputs: &[(&str, serde_json::Value)],
    ) {
        let mut log =
            SessionLog::create(dir, model, None, Masker::empty(), created_ms).expect("log");
        let mut blocks = Vec::new();
        for (i, (name, input)) in inputs.iter().enumerate() {
            blocks.push(
                json!({"type": "tool_use", "id": format!("t{i}"), "name": name, "input": input}),
            );
        }
        log.append(
            &EntryPayload::Item {
                item: Item::Assistant {
                    blocks,
                    model: None,
                },
            },
            created_ms,
        )
        .expect("append");
        log.append(
            &EntryPayload::Item {
                item: Item::ToolResults {
                    results: inputs
                        .iter()
                        .enumerate()
                        .map(|(i, _)| ToolResultItem {
                            tool_use_id: format!("t{i}"),
                            content: "ok".into(),
                            is_error: false,
                        })
                        .collect(),
                },
            },
            created_ms,
        )
        .expect("append");
    }

    const NOW: u64 = 1_800_000_000_000;

    #[test]
    fn audit_counts_per_tool_and_class_over_a_replayed_session() {
        let dir = tempfile::tempdir().unwrap();
        session_with(
            dir.path(),
            "claude-opus-5",
            NOW - 1000,
            &[
                ("read", json!({"path": "a.rs"})),
                ("read", json!({"path": "b.rs", "limit": "5"})),
                ("grep", json!({"pattern": "x", "files_only": "true"})),
                ("some_mcp_tool", json!({"anything": 1})),
            ],
        );
        let r = run(dir.path(), 7, NOW);
        assert_eq!(r.sessions, 1);
        assert_eq!(r.builtin_calls, 3);
        assert_eq!(r.other_calls, 1);
        assert_eq!(r.calls_touched, 2);
        assert_eq!(r.by_tool["read"]["type"], 1);
        assert_eq!(r.by_tool["grep"]["type"], 1);
        assert!((r.touched_pct() - 66.666).abs() < 0.01);
        let table = r.render();
        assert!(
            table.contains("read              type                1"),
            "{table}"
        );
        assert!(table.contains("66.67%"), "{table}");
    }

    #[test]
    fn audit_counts_parse_fatal_turn_ends() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create(
            dir.path(),
            "claude-opus-5",
            None,
            Masker::empty(),
            NOW - 1000,
        )
        .expect("log");
        log.append(
            &EntryPayload::Cancelled {
                reason: "error: stream parse error: tool input didn't parse at block 1".into(),
            },
            NOW,
        )
        .expect("append");
        log.append(
            &EntryPayload::Cancelled {
                reason: "interrupted by the user".into(),
            },
            NOW,
        )
        .expect("append");
        let r = run(dir.path(), 7, NOW);
        assert_eq!(r.parse_fatal, 1);
        assert!(r
            .render()
            .contains("parse-fatal turn ends (json-repair could not save the body): 1"));
    }

    #[test]
    fn audit_skips_the_test_model_and_old_sessions() {
        let dir = tempfile::tempdir().unwrap();
        session_with(
            dir.path(),
            "m",
            NOW - 1000,
            &[("read", json!({"limit": "5"}))],
        );
        session_with(
            dir.path(),
            "claude-opus-5",
            NOW - 30 * 24 * 3600 * 1000,
            &[("read", json!({"limit": "5"}))],
        );
        let r = run(dir.path(), 7, NOW);
        assert_eq!(r.sessions, 0);
        assert_eq!(r.skipped_sessions, 2);
        assert_eq!(r.builtin_calls, 0);
        assert!(r.render().contains("no mismatches"));
    }
}
