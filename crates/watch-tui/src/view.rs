use crate::app::AppState;
use hotl_theme::Palette;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use watch_types::{AgentObservation, Status};

#[derive(Debug, PartialEq, Eq)]
pub enum Row {
    Agent {
        glyph: String,
        name: String,
        subtitle: String,
        status: Status,
        selected: bool,
    },
    Spacer,
}

// Some(count) only when agents span more than one session; else None.
fn session_summary(agents: &[AgentObservation]) -> Option<String> {
    let sessions: std::collections::BTreeSet<&str> =
        agents.iter().map(|o| o.location.group.as_str()).collect();
    match sessions.len() {
        0 | 1 => None,
        n => Some(format!("{n} sessions")),
    }
}

fn project_name(p: &str) -> String {
    p.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(p)
        .to_string()
}

// Working animation: an equalizer-style bar rising and falling.
// Working animation: a braille "snake" that wanders the cell, its body
// breathing between 2 and 3 dots. Deterministic (indexed by tick).
const WORKING_FRAMES: [&str; 16] = [
    "⠑", "⠔", "⣄", "⣠", "⡠", "⠢", "⠚", "⠜", "⠔", "⠤", "⣠", "⣄", "⢄", "⠆", "⠃", "⠑",
];

fn glyph_for(status: Status, tick: u32) -> String {
    match status {
        Status::Working => WORKING_FRAMES[(tick as usize) % WORKING_FRAMES.len()].to_string(),
        Status::Blocked => "!".to_string(),
        Status::Idle => "√".to_string(),
        Status::Unknown => "·".to_string(),
    }
}

pub fn rows(agents: &[AgentObservation], cursor: usize, tick: u32) -> Vec<Row> {
    let mut out = Vec::new();
    for (idx, o) in agents.iter().enumerate() {
        out.push(Row::Agent {
            glyph: glyph_for(o.status, tick),
            name: project_name(&o.cwd),
            subtitle: o
                .status_line
                .clone()
                .unwrap_or_else(|| o.agent.name.clone()),
            status: o.status,
            selected: idx == cursor,
        });
        out.push(Row::Spacer);
    }
    out
}

fn status_color(p: &Palette, s: Status) -> Color {
    match s {
        Status::Working => p.active,
        Status::Blocked => p.blocked,
        Status::Idle => p.idle,
        Status::Unknown => p.faint,
    }
}

// Wordmark, plus a session breadcrumb only when spanning >1 session.
fn title_line(agents: &[AgentObservation], p: &Palette) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            " ◆ ",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "hotl",
            Style::default().fg(p.ink).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(sess) = session_summary(agents) {
        spans.push(Span::styled("  ·  ", Style::default().fg(p.faint)));
        spans.push(Span::styled(
            format!("⧉ {sess}"),
            Style::default().fg(p.muted),
        ));
    }
    Line::from(spans)
}

pub fn view(state: &AppState, p: &Palette, frame: &mut Frame) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(title_line(&state.agents, p)), chunks[0]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.faint));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    if state.agents.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  no agents found",
                Style::default().fg(p.muted),
            ))),
            inner,
        );
    } else {
        let lines: Vec<Line> = rows(&state.agents, state.cursor, state.spinner_tick)
            .iter()
            .map(|r| row_line(r, p, inner.width))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {}", state.status), Style::default().fg(p.muted)),
            Span::styled(
                "   j/k move · enter jump · r refresh · q quit",
                Style::default().fg(p.faint),
            ),
        ])),
        chunks[2],
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".into();
    }
    let kept: String = s.chars().take(max - 1).collect();
    format!("{kept}…")
}

fn row_line(row: &Row, p: &Palette, width: u16) -> Line<'static> {
    match row {
        Row::Spacer => Line::from(""),
        Row::Agent {
            glyph,
            name,
            subtitle,
            status,
            selected,
        } => {
            let gcolor = status_color(p, *status);
            let base = if *selected {
                Style::default().bg(p.band)
            } else {
                Style::default()
            };
            let head = 1 + 1 + 1 + name.chars().count() + 3;
            let avail = (width as usize).saturating_sub(head);
            let sub = truncate(subtitle, avail);
            let pad = avail.saturating_sub(sub.chars().count());
            Line::from(vec![
                Span::styled(" ", base),
                Span::styled(glyph.clone(), base.fg(gcolor).add_modifier(Modifier::BOLD)),
                Span::styled(" ", base),
                Span::styled(name.clone(), base.fg(p.ink).add_modifier(Modifier::BOLD)),
                Span::styled(format!("   {sub}{}", " ".repeat(pad)), base.fg(p.muted)),
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use watch_types::{Agent, Location, LocationHandle, Source};

    fn obs(group: &str, sub: &str, cwd: &str, status: Status) -> AgentObservation {
        AgentObservation {
            agent: Agent {
                name: "claude".into(),
                pid: 1,
                argv: "claude".into(),
            },
            cwd: cwd.into(),
            status,
            status_line: None,
            location: Location {
                group: group.into(),
                sub_group: Some(sub.into()),
                handle: LocationHandle::Tmux {
                    pane_id: cwd.into(),
                    session: group.into(),
                    window_index: 0,
                },
            },
            source: Source::Tmux,
        }
    }

    fn obs_sl(
        group: &str,
        sub: &str,
        cwd: &str,
        status: Status,
        sl: Option<&str>,
    ) -> AgentObservation {
        let mut o = obs(group, sub, cwd, status);
        o.status_line = sl.map(|s| s.to_string());
        o
    }

    #[test]
    fn rows_are_flat_no_group_headers() {
        // Session context moved to the title bar; the list has only agents/spacers.
        let rs = rows(&[obs("base-0", "w (0)", "/tmp/a", Status::Idle)], 0, 0);
        assert!(rs.iter().any(|r| matches!(r, Row::Agent { .. })));
        assert!(rs
            .iter()
            .all(|r| matches!(r, Row::Agent { .. } | Row::Spacer)));
    }

    #[test]
    fn session_summary_hidden_for_single_session() {
        let a = vec![
            obs("base-0", "w", "/tmp/a", Status::Idle),
            obs("base-0", "w", "/tmp/b", Status::Idle),
        ];
        assert_eq!(session_summary(&a), None);
        assert_eq!(session_summary(&[]), None);
    }

    #[test]
    fn session_summary_counts_multiple_sessions() {
        let a = vec![
            obs("base-0", "w", "/tmp/a", Status::Idle),
            obs("work", "w", "/tmp/b", Status::Idle),
            obs("work", "w", "/tmp/c", Status::Idle),
        ];
        assert_eq!(session_summary(&a).as_deref(), Some("2 sessions"));
    }

    #[test]
    fn title_is_just_wordmark_for_single_session() {
        let pal = Palette::default();
        let a = vec![
            obs("base-0", "w", "/tmp/a", Status::Blocked),
            obs("base-0", "w", "/tmp/b", Status::Idle),
        ];
        let text: String = title_line(&a, &pal)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("hotl"), "wordmark present: {text}");
        // Counts are noise in the title — they belong in the status bar.
        assert!(!text.contains("agent"), "no agent count in title: {text}");
        assert!(
            !text.contains("blocked"),
            "no blocked count in title: {text}"
        );
        assert!(
            !text.contains("session"),
            "no breadcrumb for one session: {text}"
        );
    }

    #[test]
    fn title_breadcrumb_appears_with_multiple_sessions() {
        let pal = Palette::default();
        let a = vec![
            obs("base-0", "w", "/tmp/a", Status::Idle),
            obs("work", "w", "/tmp/b", Status::Idle),
        ];
        let text: String = title_line(&a, &pal)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("2 sessions"), "breadcrumb present: {text}");
    }

    #[test]
    fn agent_named_by_project_dir_with_status_glyph() {
        let rs = rows(&[obs("g", "w (0)", "/home/me/lca", Status::Blocked)], 0, 0);
        let found = rs.iter().any(|r| matches!(r, Row::Agent { name, glyph, status: Status::Blocked, .. } if name == "lca" && glyph == "!"));
        assert!(found);
    }

    #[test]
    fn only_cursor_agent_selected() {
        let agents = vec![
            obs("g", "w (0)", "/tmp/a", Status::Idle),
            obs("g", "w (0)", "/tmp/b", Status::Idle),
        ];
        let selected = rows(&agents, 1, 0)
            .into_iter()
            .filter(|r| matches!(r, Row::Agent { selected: true, .. }))
            .count();
        assert_eq!(selected, 1);
    }

    #[test]
    fn subtitle_is_status_line_when_present() {
        let a = obs_sl(
            "g",
            "w (0)",
            "/tmp/lca",
            Status::Idle,
            Some("[I] .../lca [main] ctx:9%"),
        );
        let rs = rows(&[a], 0, 0);
        assert!(rs.iter().any(|r| matches!(r,
            Row::Agent { subtitle, .. } if subtitle == "[I] .../lca [main] ctx:9%")));
    }

    #[test]
    fn subtitle_falls_back_to_agent_name() {
        let a = obs_sl("g", "w (0)", "/tmp/lca", Status::Idle, None);
        let rs = rows(&[a], 0, 0);
        assert!(rs.iter().any(|r| matches!(r,
            Row::Agent { subtitle, .. } if subtitle == "claude")));
    }

    fn agent_glyph(o: &AgentObservation, tick: u32) -> String {
        rows(std::slice::from_ref(o), 0, tick)
            .into_iter()
            .find_map(|r| match r {
                Row::Agent { glyph, .. } => Some(glyph),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn working_glyph_animates_with_tick() {
        let a = obs_sl("g", "w (0)", "/tmp/lca", Status::Working, None);
        assert_ne!(agent_glyph(&a, 0), agent_glyph(&a, 1));
    }

    #[test]
    fn non_working_glyph_is_static_across_ticks() {
        let a = obs_sl("g", "w (0)", "/tmp/lca", Status::Idle, None);
        assert_eq!(agent_glyph(&a, 0), agent_glyph(&a, 5));
    }
}
