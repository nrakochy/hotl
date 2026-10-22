//! `hotl stats [--days N] [--json]`: sessions, turns, tool calls, tokens and
//! cost across every saved session, by model and by day. A fold over the
//! logs, no index.

use std::collections::BTreeMap;
use std::io::BufRead;

use hotl_types::{Entry, EntryPayload, Item, TokenUsage};

const USAGE: &str = "usage: hotl stats [--days N] [--json]";

/// Five counters, shared by the totals and by both tables.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Row {
    pub sessions: u64,
    pub turns: u64,
    pub tool_calls: u64,
    pub usage: TokenUsage,
    /// `None` when any session in the row has an uncatalogued model — a
    /// partial sum never pretends to be the total.
    pub cost_usd: Option<f64>,
}

impl Row {
    fn add(&mut self, other: &Row) {
        // An empty row is the identity: its `None` cost is "nothing yet",
        // not "uncatalogued".
        self.cost_usd = match (self.sessions, self.cost_usd, other.cost_usd) {
            (0, _, c) => c,
            (_, Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        self.sessions += other.sessions;
        self.turns += other.turns;
        self.tool_calls += other.tool_calls;
        self.usage += other.usage;
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Stats {
    #[serde(flatten)]
    pub totals: Row,
    pub by_model: BTreeMap<String, Row>,
    pub by_day: BTreeMap<String, Row>,
    /// Lines that did not parse (a torn tail, a future entry kind).
    pub skipped: u64,
}

/// One log's counters, or `None` for a file without a readable header.
fn fold_log(path: &std::path::Path, skipped: &mut u64) -> Option<(String, String, Row)> {
    let file = std::fs::File::open(path).ok()?;
    let mut row = Row {
        sessions: 1,
        ..Default::default()
    };
    let mut model: Option<String> = None;
    let mut day: Option<String> = None;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            *skipped += 1;
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
            *skipped += 1;
            continue;
        };
        match entry.payload {
            EntryPayload::Header { header } => {
                model = Some(header.model);
                day = Some(hotl_context::civil_date_utc(header.created_at_ms));
            }
            EntryPayload::Usage {
                usage,
                request_id: None,
            } => row.usage += usage,
            EntryPayload::Item { item } => match item {
                Item::User {
                    synthetic: None, ..
                } => row.turns += 1,
                Item::Assistant { blocks } => {
                    row.tool_calls += blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                        .count() as u64;
                }
                _ => {}
            },
            _ => {}
        }
    }
    let model = model?;
    row.cost_usd = hotl_provider::catalog::cost_usd(&model, &row.usage);
    Some((model, day?, row))
}

/// Fold every log under `sessions_dir` whose header is at or after `since_ms`.
pub fn fold(sessions_dir: &std::path::Path, since_ms: Option<u64>) -> Stats {
    let mut stats = Stats::default();
    for (_, path, _) in hotl_store::list_sessions(sessions_dir) {
        // A cheap header peek first: the day filter should not read whole logs.
        if let Some(since) = since_ms {
            match created_at_ms(&path) {
                Some(ms) if ms >= since => {}
                _ => continue,
            }
        }
        let Some((model, day, row)) = fold_log(&path, &mut stats.skipped) else {
            continue;
        };
        stats.totals.add(&row);
        stats.by_model.entry(model).or_default().add(&row);
        stats.by_day.entry(day).or_default().add(&row);
    }
    stats
}

fn created_at_ms(path: &std::path::Path) -> Option<u64> {
    let file = std::fs::File::open(path).ok()?;
    let first = std::io::BufReader::new(file).lines().next()?.ok()?;
    match serde_json::from_str::<Entry>(&first).ok()?.payload {
        EntryPayload::Header { header } => Some(header.created_at_ms),
        _ => None,
    }
}

fn cost(c: Option<f64>) -> String {
    match c {
        Some(c) => format!("${c:.2}"),
        None => "—".to_string(),
    }
}

fn summary_line(r: &Row) -> String {
    format!(
        "{} sessions · {} turns · {} tool calls · {} in / {} out · {}",
        r.sessions,
        r.turns,
        r.tool_calls,
        hotl_tui::app::tok(r.usage.input_tokens + r.usage.cache_read_input_tokens),
        hotl_tui::app::tok(r.usage.output_tokens),
        cost(r.cost_usd)
    )
}

fn table(title: &str, rows: &BTreeMap<String, Row>) -> String {
    let width = rows
        .keys()
        .map(|k| k.len())
        .max()
        .unwrap_or(5)
        .max(title.len());
    let mut out = format!(
        "{title:<width$}  sessions  turns  tools  {:>8}  {:>8}  {:>9}\n",
        "in", "out", "cost"
    );
    for (k, r) in rows {
        out.push_str(&format!(
            "{k:<width$}  {:>8}  {:>5}  {:>5}  {:>8}  {:>8}  {:>9}\n",
            r.sessions,
            r.turns,
            r.tool_calls,
            hotl_tui::app::tok(r.usage.input_tokens + r.usage.cache_read_input_tokens),
            hotl_tui::app::tok(r.usage.output_tokens),
            cost(r.cost_usd)
        ));
    }
    out
}

pub(crate) fn render_text(stats: &Stats) -> String {
    let mut out = summary_line(&stats.totals);
    out.push('\n');
    if stats.totals.cost_usd.is_none() && stats.totals.sessions > 0 {
        out.push_str("(cost: — means a session's model is not in the price catalog)\n");
    }
    if !stats.by_model.is_empty() {
        out.push('\n');
        out.push_str(&table("model", &stats.by_model));
        out.push('\n');
        out.push_str(&table("day", &stats.by_day));
    }
    if stats.skipped > 0 {
        out.push_str(&format!(
            "\n{} line(s) could not be read and were skipped\n",
            stats.skipped
        ));
    }
    out
}

pub fn stats_main(args: &[String]) -> i32 {
    let mut days: Option<u64> = None;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--json" => json = true,
            "--days" => match iter.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(n) => days = Some(n),
                None => {
                    eprintln!("hotl stats: --days needs a whole number of days\n{USAGE}");
                    return 2;
                }
            },
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    let since = days.map(|d| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now.saturating_sub(d * 86_400_000)
    });
    let stats = fold(&crate::agent::sessions_dir(), since);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&stats).unwrap_or_else(|_| "{}".into())
        );
    } else {
        print!("{}", render_text(&stats));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_store::{Masker, SessionLog};

    fn log_with(
        dir: &std::path::Path,
        model: &str,
        created: u64,
        usage: &[(u64, u64)],
        synthetic_turn: bool,
    ) -> SessionLog {
        let mut log = SessionLog::create(dir, model, None, Masker::empty(), created).unwrap();
        let push = |log: &mut SessionLog, p: EntryPayload| log.append(&p, created + 1).unwrap();
        if synthetic_turn {
            push(
                &mut log,
                EntryPayload::Item {
                    item: Item::User {
                        text: "<memory/>".into(),
                        synthetic: Some(hotl_types::SyntheticReason::Memory),
                        images: Vec::new(),
                    },
                },
            );
        }
        for (input, output) in usage {
            push(
                &mut log,
                EntryPayload::Item {
                    item: Item::User {
                        text: "ask".into(),
                        synthetic: None,
                        images: Vec::new(),
                    },
                },
            );
            push(
                &mut log,
                EntryPayload::Item {
                    item: Item::Assistant {
                        blocks: vec![
                            serde_json::json!({"type":"tool_use","id":"a","name":"read","input":{}}),
                            serde_json::json!({"type":"text","text":"ok"}),
                        ],
                    },
                },
            );
            push(
                &mut log,
                EntryPayload::Usage {
                    usage: TokenUsage {
                        input_tokens: *input,
                        output_tokens: *output,
                        ..Default::default()
                    },
                    request_id: None,
                },
            );
        }
        log
    }

    #[test]
    fn stats_sums_usage_entries_and_counts_real_turns_only() {
        let dir = tempfile::tempdir().unwrap();
        let _a = log_with(
            dir.path(),
            "claude-opus-4-8",
            1_000,
            &[(100, 10), (200, 20)],
            true,
        );
        let _b = log_with(dir.path(), "claude-opus-4-8", 2_000, &[(50, 5)], false);
        let s = fold(dir.path(), None);
        assert_eq!(s.totals.sessions, 2);
        assert_eq!(s.totals.turns, 3, "synthetic items are not turns");
        assert_eq!(s.totals.tool_calls, 3);
        assert_eq!(s.totals.usage.input_tokens, 350);
        assert_eq!(s.totals.usage.output_tokens, 35);
        assert!(s.totals.cost_usd.is_some());
        assert_eq!(s.skipped, 0);
        assert_eq!(s.by_model.len(), 1);
        assert_eq!(s.by_day["1970-01-01"].sessions, 2);
    }

    #[test]
    fn stats_prices_at_the_header_model_and_blanks_an_uncatalogued_one() {
        let dir = tempfile::tempdir().unwrap();
        let _a = log_with(dir.path(), "claude-opus-4-8", 1_000, &[(100, 10)], false);
        let _b = log_with(dir.path(), "gateway-alias", 1_000, &[(100, 10)], false);
        let s = fold(dir.path(), None);
        assert!(s.by_model["claude-opus-4-8"].cost_usd.is_some());
        assert!(s.by_model["gateway-alias"].cost_usd.is_none());
        assert!(s.totals.cost_usd.is_none(), "a partial sum is not a total");
        let text = render_text(&s);
        assert!(text.contains(" · —\n"), "{text}");
        assert!(text.contains("not in the price catalog"));
    }

    #[test]
    fn stats_days_filters_by_header_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let now = 10 * 86_400_000;
        let _old = log_with(
            dir.path(),
            "claude-opus-4-8",
            now - 3 * 86_400_000,
            &[(1, 1)],
            false,
        );
        let _new = log_with(
            dir.path(),
            "claude-opus-4-8",
            now - 3_600_000,
            &[(1, 1)],
            false,
        );
        let s = fold(dir.path(), Some(now - 86_400_000));
        assert_eq!(s.sessions_total(), 1);
    }

    impl Stats {
        fn sessions_total(&self) -> u64 {
            self.totals.sessions
        }
    }

    #[test]
    fn stats_json_carries_the_three_tables() {
        let dir = tempfile::tempdir().unwrap();
        let _a = log_with(dir.path(), "claude-opus-4-8", 1_000, &[(100, 10)], false);
        let v = serde_json::to_value(fold(dir.path(), None)).unwrap();
        assert_eq!(v["sessions"], 1);
        assert!(v["by_model"]["claude-opus-4-8"]["cost_usd"].is_number());
        assert_eq!(v["by_day"]["1970-01-01"]["turns"], 1);
        assert_eq!(v["skipped"], 0);
    }

    #[test]
    fn a_torn_last_line_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let log = log_with(dir.path(), "claude-opus-4-8", 1_000, &[(100, 10)], false);
        let path = log.path().to_path_buf();
        drop(log);
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"id\":\"half");
        std::fs::write(&path, raw).unwrap();
        let s = fold(dir.path(), None);
        assert_eq!(s.totals.sessions, 1);
        assert_eq!(s.totals.turns, 1);
        assert_eq!(s.skipped, 1);
        assert!(render_text(&s).contains("1 line(s) could not be read"));
    }

    #[test]
    fn stats_main_rejects_a_bad_days_value() {
        assert_eq!(stats_main(&["--days".to_string(), "x".to_string()]), 2);
        assert_eq!(stats_main(&["--what".to_string()]), 2);
    }
}
