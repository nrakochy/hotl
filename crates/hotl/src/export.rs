//! `hotl export <session> [--md|--html]`: a past session's transcript — the
//! replayed projection, what the model saw, in order — to stdout. Secrets are
//! re-masked on the way out, so a value that became a secret after the write
//! is still absent.

use hotl_store::{Masker, Replayed};
use hotl_types::{Item, SyntheticReason};

use crate::agent::{resolve_session_ref, sessions_dir, sessions_newest_first};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Markdown,
    Html,
}

const USAGE: &str = "usage: hotl export <id|name|@last> [--md|--html]";

pub fn export_main(args: &[String]) -> i32 {
    let mut session: Option<&str> = None;
    let mut format = Format::Markdown;
    for a in args {
        match a.as_str() {
            "--md" | "--markdown" => format = Format::Markdown,
            "--html" => format = Format::Html,
            other if other.starts_with('-') || session.is_some() => {
                eprintln!("{USAGE}");
                return 2;
            }
            other => session = Some(other),
        }
    }
    let Some(session) = session else {
        eprintln!("{USAGE}");
        return 2;
    };
    let dir = sessions_dir();
    let id = match resolve_session_ref(session, &sessions_newest_first(&dir)) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("hotl export: {e}");
            return 2;
        }
    };
    match export(&dir, &id, format, &Masker::from_env()) {
        Ok((text, warnings)) => {
            for w in warnings {
                eprintln!("hotl export: {w}");
            }
            print!("{text}");
            0
        }
        Err(e) => {
            eprintln!("hotl export: {e}");
            1
        }
    }
}

/// The rendered transcript and replay's integrity warnings, masked.
fn export(
    dir: &std::path::Path,
    id: &str,
    format: Format,
    masker: &Masker,
) -> Result<(String, Vec<String>), String> {
    let replayed = hotl_store::replay_chain(dir, id)?;
    let text = match format {
        Format::Markdown => render_md(&replayed),
        Format::Html => render_html(&replayed),
    };
    Ok((masker.apply(&text), replayed.warnings.clone()))
}

/// Plain words for a synthetic user item's origin.
fn label(reason: &SyntheticReason) -> &'static str {
    match reason {
        SyntheticReason::ProjectInstructions => "project instructions",
        SyntheticReason::SystemReminder => "system reminder",
        SyntheticReason::Steer => "steer",
        SyntheticReason::CompactionSummary => "compaction summary",
        SyntheticReason::SubagentResult => "sub-agent result",
        SyntheticReason::DoomLoopNudge => "doom-loop nudge",
        SyntheticReason::RetryFeedback => "retry feedback",
        SyntheticReason::Moim => "moim",
        SyntheticReason::Memory => "memory",
        SyntheticReason::SubdirInstructions => "subdirectory instructions",
        SyntheticReason::Todos => "todos",
        SyntheticReason::Environment => "environment",
        SyntheticReason::GoalGuidance => "goal guidance",
        SyntheticReason::Misprediction => "misprediction",
        SyntheticReason::Unknown => "synthetic",
    }
}

/// One section per item: `(heading, body)`. Thinking blocks are omitted;
/// `tool_use` blocks are JSON; tool results are one block each.
fn sections(r: &Replayed) -> Vec<(String, Vec<Block>)> {
    let mut out = Vec::new();
    for item in &r.items {
        match item {
            Item::System { text } => out.push(("system".into(), vec![Block::Text(text.clone())])),
            Item::User {
                text, synthetic, ..
            } => {
                let heading = match synthetic {
                    Some(reason) => format!("user ({})", label(reason)),
                    None => "user".into(),
                };
                out.push((heading, vec![Block::Text(text.clone())]));
            }
            Item::Assistant { blocks } => {
                let mut body = Vec::new();
                for b in blocks {
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                body.push(Block::Text(t.to_string()));
                            }
                        }
                        Some("tool_use") => body.push(Block::Fenced {
                            lang: "json",
                            title: b
                                .get("name")
                                .and_then(|n| n.as_str())
                                .map(|n| format!("tool_use {n}")),
                            text: serde_json::to_string_pretty(b).unwrap_or_default(),
                        }),
                        // Thinking is the model's scratch space, not the transcript.
                        Some("thinking") | Some("redacted_thinking") => {}
                        _ => body.push(Block::Fenced {
                            lang: "json",
                            title: None,
                            text: serde_json::to_string_pretty(b).unwrap_or_default(),
                        }),
                    }
                }
                out.push(("assistant".into(), body));
            }
            Item::ToolResults { results } => {
                let body = results
                    .iter()
                    .map(|res| Block::Fenced {
                        lang: "",
                        title: Some(format!(
                            "{}{}",
                            res.tool_use_id,
                            if res.is_error { " (error)" } else { "" }
                        )),
                        text: res.content.clone(),
                    })
                    .collect();
                out.push(("tool results".into(), body));
            }
            Item::Unknown => {}
        }
    }
    out
}

enum Block {
    Text(String),
    Fenced {
        lang: &'static str,
        title: Option<String>,
        text: String,
    },
}

fn header_lines(r: &Replayed) -> Vec<(&'static str, String)> {
    let h = &r.header;
    let mut lines = vec![("id", h.session_id.clone())];
    if let Some(n) = &r.name {
        lines.push(("name", n.clone()));
    }
    lines.push(("model", h.model.clone()));
    lines.push(("created", hotl_context::civil_date_utc(h.created_at_ms)));
    if let Some(p) = &h.parent_session_id {
        lines.push(("parent", p.clone()));
    }
    lines
}

pub(crate) fn render_md(r: &Replayed) -> String {
    let mut out = String::new();
    out.push_str("# hotl session\n\n");
    for (k, v) in header_lines(r) {
        out.push_str(&format!("- {k}: {v}\n"));
    }
    for (heading, body) in sections(r) {
        out.push_str(&format!("\n## {heading}\n\n"));
        for block in body {
            match block {
                Block::Text(t) => {
                    out.push_str(t.trim_end());
                    out.push_str("\n\n");
                }
                Block::Fenced { lang, title, text } => {
                    if let Some(t) = title {
                        out.push_str(&format!("**{t}**\n\n"));
                    }
                    // Four backticks so a body holding three stays intact.
                    out.push_str(&format!("````{lang}\n{}\n````\n\n", text.trim_end()));
                }
            }
        }
    }
    out
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn render_html(r: &Replayed) -> String {
    let title = r
        .name
        .clone()
        .unwrap_or_else(|| r.header.session_id.clone());
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str(&format!("<title>hotl: {}</title>\n", escape(&title)));
    out.push_str(
        "<style>\n\
         body { font: 15px/1.5 system-ui, sans-serif; max-width: 60em; margin: 2em auto; padding: 0 1em; }\n\
         pre { white-space: pre-wrap; word-break: break-word; background: #f4f4f4; padding: .75em; }\n\
         section { margin: 1.5em 0; border-top: 1px solid #ddd; padding-top: .5em; }\n\
         h2 { font-size: 1em; text-transform: uppercase; letter-spacing: .05em; color: #555; }\n\
         .assistant pre.text { background: none; padding: 0; }\n\
         dl { display: grid; grid-template-columns: max-content auto; gap: .2em 1em; }\n\
         dt { color: #555; }\n\
         </style>\n</head>\n<body>\n<article>\n",
    );
    out.push_str("<h1>hotl session</h1>\n<dl>\n");
    for (k, v) in header_lines(r) {
        out.push_str(&format!("<dt>{k}</dt><dd>{}</dd>\n", escape(&v)));
    }
    out.push_str("</dl>\n");
    for (heading, body) in sections(r) {
        let class = heading
            .split(|c: char| !c.is_alphanumeric())
            .next()
            .unwrap_or("item");
        out.push_str(&format!(
            "<section class=\"{class}\">\n<h2>{}</h2>\n",
            escape(&heading)
        ));
        for block in body {
            match block {
                Block::Text(t) => {
                    out.push_str(&format!(
                        "<pre class=\"text\">{}</pre>\n",
                        escape(t.trim_end())
                    ));
                }
                Block::Fenced { title, text, .. } => {
                    if let Some(t) = title {
                        out.push_str(&format!("<p><b>{}</b></p>\n", escape(&t)));
                    }
                    out.push_str(&format!("<pre>{}</pre>\n", escape(text.trim_end())));
                }
            }
        }
        out.push_str("</section>\n");
    }
    out.push_str("</article>\n</body>\n</html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_store::SessionLog;
    use hotl_types::{EntryPayload, ToolResultItem};

    fn push(log: &mut SessionLog, item: Item) {
        log.append(&EntryPayload::Item { item }, 2).unwrap();
    }

    fn three_items(dir: &std::path::Path, masker: Masker) -> SessionLog {
        let mut log = SessionLog::create(dir, "m", None, masker, 1).unwrap();
        push(
            &mut log,
            Item::User {
                text: "hello".into(),
                synthetic: None,
                images: Vec::new(),
            },
        );
        push(
            &mut log,
            Item::Assistant {
                blocks: vec![
                    serde_json::json!({"type":"thinking","thinking":"private"}),
                    serde_json::json!({"type":"text","text":"hi"}),
                    serde_json::json!({"type":"tool_use","id":"t1","name":"read","input":{"path":"a"}}),
                ],
            },
        );
        push(
            &mut log,
            Item::ToolResults {
                results: vec![ToolResultItem {
                    tool_use_id: "t1".into(),
                    content: "<script>alert(1)</script> and vk-live-abcdefghij".into(),
                    is_error: true,
                }],
            },
        );
        log
    }

    #[test]
    fn markdown_export_has_one_section_per_item_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let log = three_items(dir.path(), Masker::empty());
        let (md, warnings) = export(
            dir.path(),
            &log.session_id,
            Format::Markdown,
            &Masker::empty(),
        )
        .unwrap();
        assert!(warnings.is_empty());
        assert!(md.starts_with("# hotl session\n\n- id: "), "{md}");
        let headings: Vec<&str> = md.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(headings, ["## user", "## assistant", "## tool results"]);
        assert!(md.contains("**t1 (error)**"));
    }

    #[test]
    fn export_renders_the_chain_not_one_log() {
        let dir = tempfile::tempdir().unwrap();
        let parent = three_items(dir.path(), Masker::empty());
        let tip = hotl_store::replay(parent.path()).unwrap().tip_entry_id;
        let child = SessionLog::create(
            dir.path(),
            "m",
            Some(hotl_store::ParentRef {
                session_id: parent.session_id.clone(),
                tip_entry_id: tip,
            }),
            Masker::empty(),
            5,
        )
        .unwrap();
        let (md, _) = export(
            dir.path(),
            &child.session_id,
            Format::Markdown,
            &Masker::empty(),
        )
        .unwrap();
        assert!(md.contains("## user\n\nhello"), "{md}");
        assert!(md.contains(&format!("- parent: {}", parent.session_id)));
    }

    #[test]
    fn export_masks_secrets_that_became_secret_after_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let log = three_items(dir.path(), Masker::empty());
        let raw = std::fs::read_to_string(log.path()).unwrap();
        assert!(raw.contains("vk-live-abcdefghij"), "written in the clear");
        let later = Masker::empty().with_value("VENDOR_KEY", "vk-live-abcdefghij");
        let (md, _) = export(dir.path(), &log.session_id, Format::Markdown, &later).unwrap();
        assert!(!md.contains("vk-live-abcdefghij"), "{md}");
    }

    #[test]
    fn html_export_escapes_markup_in_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let log = three_items(dir.path(), Masker::empty());
        let (html, _) =
            export(dir.path(), &log.session_id, Format::Html, &Masker::empty()).unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"));
        assert_eq!(html.matches("<article>").count(), 1);
        assert!(html.starts_with("<!doctype html>"));
    }

    #[test]
    fn thinking_blocks_are_omitted_and_tool_use_blocks_are_fenced_json() {
        let dir = tempfile::tempdir().unwrap();
        let log = three_items(dir.path(), Masker::empty());
        let (md, _) = export(
            dir.path(),
            &log.session_id,
            Format::Markdown,
            &Masker::empty(),
        )
        .unwrap();
        assert!(!md.contains("private"), "thinking leaked: {md}");
        assert!(md.contains("**tool_use read**\n\n````json\n{"), "{md}");
        assert!(md.contains("\"name\": \"read\""));
    }

    #[test]
    fn export_of_an_unknown_session_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let _log = three_items(dir.path(), Masker::empty());
        let err = resolve_session_ref("nope", &sessions_newest_first(dir.path())).unwrap_err();
        assert_eq!(err, "no session matches `nope`");
        // And the CLI maps a missing argument to the same exit.
        assert_eq!(export_main(&["--html".to_string()]), 2);
        assert_eq!(export_main(&["a".to_string(), "b".to_string()]), 2);
    }
}
