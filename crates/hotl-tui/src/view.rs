//! Pure view: transcript viewport, activity strip, bordered input, hint row,
//! plus the ask modal and help overlay. Renders only from `State` — no clocks,
//! no I/O. Theme is deliberately 3 fields and local to this crate (a shared
//! theme crate with watch is future work).

use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::anim;
use crate::app::{Phase, Scroll, State, ToolStatus, TranscriptItem};
use crate::vim::Mode;

pub struct Theme {
    pub accent: Color,
    pub dim: Color,
    pub fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme { accent: Color::Cyan, dim: Color::DarkGray, fg: Color::Gray }
    }
}

const SPIN: [&str; 4] = ["◐", "◓", "◑", "◒"];

pub fn view(state: &State, theme: &Theme, frame: &mut Frame) {
    let [transcript, strip, input, hint] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    render_transcript(state, theme, frame, transcript);
    render_strip(state, theme, frame, strip);
    render_input(state, theme, frame, input);
    render_hint(state, theme, frame, hint);
    if matches!(state.phase, Phase::WaitingAsk { .. }) {
        render_ask(state, theme, frame, transcript);
    }
    if state.help_open {
        render_help(theme, frame, transcript);
    }
}

fn render_transcript(state: &State, theme: &Theme, frame: &mut Frame, area: Rect) {
    let lines: Vec<Line> = state.transcript.iter().flat_map(|i| item_lines(i, theme)).collect();
    let skip = match state.scroll {
        Scroll::Follow => lines.len().saturating_sub(area.height as usize),
        Scroll::At(item) => state.transcript[..item.min(state.transcript.len())]
            .iter()
            .map(|i| item_lines(i, theme).len())
            .sum::<usize>()
            .min(lines.len().saturating_sub(1)),
    };
    frame.render_widget(Paragraph::new(lines).scroll((skip as u16, 0)), area);
}

fn item_lines<'a>(item: &TranscriptItem, theme: &Theme) -> Vec<Line<'a>> {
    match item {
        TranscriptItem::User { text } => text
            .split('\n')
            .enumerate()
            .map(|(i, l)| {
                let prefix = if i == 0 { "❯ " } else { "  " };
                Line::styled(format!("{prefix}{l}"), Style::new().fg(theme.fg).bold())
            })
            .collect(),
        TranscriptItem::Steer { text, queued: true } => vec![Line::styled(
            format!("⤷ {text} — steer queued, applies at next step"),
            Style::new().fg(theme.dim),
        )],
        TranscriptItem::Steer { text, queued: false } => {
            vec![Line::styled(format!("⤷ {text}"), Style::new().fg(theme.accent))]
        }
        TranscriptItem::Assistant { text } => text.split('\n').map(|l| Line::raw(l.to_string())).collect(),
        TranscriptItem::Tool { name, summary, status, ticks } => {
            let marker = match status {
                ToolStatus::Running | ToolStatus::AutoAllowed { .. } => SPIN[(*ticks % 4) as usize],
                ToolStatus::Done => "✓",
                ToolStatus::Failed => "✗",
                ToolStatus::Denied => "⛔",
            };
            let mut line = format!("[{marker} {name}] {summary}");
            if let ToolStatus::AutoAllowed { rule } = status {
                line.push_str(&format!(" · auto-allowed: {rule}"));
            }
            if !matches!(status, ToolStatus::Denied) {
                line.push_str(&format!(" · {}s", ticks / 8));
            }
            vec![Line::styled(line, Style::new().fg(theme.fg))]
        }
        TranscriptItem::Notice { text } => {
            vec![Line::styled(text.clone(), Style::new().fg(theme.dim).italic())]
        }
    }
}

fn render_strip(state: &State, theme: &Theme, frame: &mut Frame, area: Rect) {
    let style = match state.phase {
        Phase::WaitingAsk { .. } => Style::new().fg(theme.accent).bold(),
        Phase::Idle => Style::new().fg(theme.dim),
        _ => Style::new().fg(theme.fg),
    };
    frame.render_widget(Paragraph::new(anim::strip_line(state)).style(style), area);
}

fn render_input(state: &State, theme: &Theme, frame: &mut Frame, area: Rect) {
    let mut block = Block::bordered().border_style(Style::new().fg(theme.dim));
    if state.vim_mode {
        let mode = match state.editor.mode() {
            Mode::Insert => "-- INSERT --",
            Mode::Normal => "-- NORMAL --",
        };
        block = block.title(mode);
    }
    let inner = block.inner(area);
    let (row, col) = state.editor.cursor();
    let text = state.editor.text();
    let line = text.split('\n').nth(row).unwrap_or("").to_string();
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(line), inner);
    if inner.width > 0 {
        let x = inner.x + (col as u16).min(inner.width - 1);
        frame.set_cursor_position((x, inner.y));
    }
}

fn render_hint(state: &State, theme: &Theme, frame: &mut Frame, area: Rect) {
    let hint = match (&state.phase, state.vim_mode, state.editor.mode()) {
        (Phase::WaitingAsk { .. }, ..) => "y allow · n deny · type a reason after n · ctrl-c cancel",
        (_, true, Mode::Normal) => "i insert · j/k scroll · ctrl-e editor · esc interrupt · ? help",
        _ => "ctrl-e editor · esc interrupt · ? help",
    };
    frame.render_widget(Paragraph::new(hint).style(Style::new().fg(theme.dim)), area);
}

fn render_ask(state: &State, theme: &Theme, frame: &mut Frame, over: Rect) {
    let Phase::WaitingAsk { summary, protected_why, input, denying, .. } = &state.phase else { return };
    let mut lines = vec![Line::styled(summary.clone(), Style::new().fg(theme.fg).bold())];
    if let Some(why) = protected_why {
        lines.push(Line::styled(format!("⚠ {why}"), Style::new().fg(theme.accent).bold()));
    }
    lines.push(Line::raw(""));
    if *denying {
        lines.push(Line::styled(format!("deny reason: {input}▏"), Style::new().fg(theme.fg)));
    } else {
        lines.push(Line::styled("y allow · n deny · type a reason after n", Style::new().fg(theme.dim)));
    }
    let area = centered(over, 60, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" waiting on you ").border_style(Style::new().fg(theme.accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_help(theme: &Theme, frame: &mut Frame, over: Rect) {
    let lines: Vec<Line> = [
        "enter send · shift/alt-enter newline",
        "esc normal mode · esc (empty) interrupt turn",
        "i a I A o O insert · h l 0 $ w b e motions (+counts)",
        "d c y operators · dd cc yy x p u",
        "j k scroll transcript when input is empty",
        "ctrl-e or :e open $EDITOR · ctrl-c quit/cancel",
        "any key closes this help",
    ]
    .into_iter()
    .map(Line::raw)
    .collect();
    let area = centered(over, 70, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" keys ").border_style(Style::new().fg(theme.accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A rect `pct`% of `over`'s width, `height` tall, centered in it.
fn centered(over: Rect, pct: u16, height: u16) -> Rect {
    let width = (over.width * pct / 100).max(10).min(over.width);
    let height = height.min(over.height);
    let x = over.x + (over.width - width) / 2;
    let y = over.y + (over.height - height) / 2;
    Rect { x, y, width, height }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::State;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(state: &State) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| view(state, &Theme::default(), f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer.cell((x, y)).unwrap().symbol()).collect())
            .collect()
    }

    // 80×24 layout: transcript rows 0-18, strip 19, input 20-22, hint 23.
    const STRIP: usize = 19;
    const INPUT_TOP: usize = 20;
    const HINT: usize = 23;

    #[test]
    fn idle_layout_shows_resting_glyph_and_hint_row() {
        let rows = draw(&State::new(true, "m".into()));
        assert!(rows[STRIP].contains("· ─ ·"), "resting glyph: {}", rows[STRIP]);
        assert!(rows[HINT].contains("? help"), "hint row: {}", rows[HINT]);
        assert!(rows[INPUT_TOP].contains("-- INSERT --"), "mode title: {}", rows[INPUT_TOP]);
    }

    #[test]
    fn waiting_ask_renders_modal_with_summary_and_protected_why() {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::WaitingAsk {
            req_id: 7,
            summary: "run bash: rm -rf ./x".into(),
            protected_why: Some("protected path".into()),
            input: String::new(),
            denying: false,
        };
        let rows = draw(&s);
        let all = rows.join("\n");
        assert!(all.contains("run bash: rm -rf ./x"), "summary in modal");
        assert!(all.contains("⚠ protected path"), "loud protected line");
        assert!(rows[STRIP].contains("waiting on you"), "halted strip");
    }

    #[test]
    fn tool_card_and_strip_share_elapsed() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::Tool {
            name: "bash".into(),
            summary: "echo hi".into(),
            status: ToolStatus::Running,
            ticks: 16,
        });
        s.phase = Phase::Tool { name: "bash".into(), ticks: 16 };
        let rows = draw(&s);
        assert!(rows[STRIP].contains("bash · 2s"), "strip elapsed: {}", rows[STRIP]);
        assert!(rows.iter().any(|r| r.contains("bash] echo hi · 2s")), "card elapsed");
    }

    #[test]
    fn steer_chip_renders_until_admitted() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::Steer { text: "go left".into(), queued: true });
        let rows = draw(&s).join("\n");
        assert!(rows.contains("⤷ go left — steer queued"), "pinned chip");
        s.transcript[0] = TranscriptItem::Steer { text: "go left".into(), queued: false };
        let rows = draw(&s).join("\n");
        assert!(rows.contains("⤷ go left"), "chip stays");
        assert!(!rows.contains("steer queued"), "queued tag gone once admitted");
    }

    #[test]
    fn normal_mode_titles_input() {
        let mut s = State::new(true, "m".into());
        s.editor.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let rows = draw(&s);
        assert!(rows[INPUT_TOP].contains("-- NORMAL --"), "{}", rows[INPUT_TOP]);
    }
}
