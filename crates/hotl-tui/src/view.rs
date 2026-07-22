//! Pure view: transcript viewport, activity strip, bordered input, hint row,
//! plus the ask modal and help overlay. Renders only from `State` — no clocks,
//! no I/O. Colors come from the shared `hotl_theme::Palette` resolved from
//! `[settings.theme]` — the same palette `hotl watch` wears. Status slots keep
//! watch's semantics: active = working, blocked = needs you, idle = settled.

use hotl_theme::Palette;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::anim;
use crate::app::{Phase, Scroll, State, ToolStatus, TranscriptItem};
use crate::vim::Mode;
use crate::wrap;

const SPIN: [&str; 4] = ["◐", "◓", "◑", "◒"];

/// How tall the input box may grow before it scrolls instead. Past this the
/// buffer is long enough that `ctrl-e` is the better tool anyway.
const INPUT_MAX_ROWS: usize = 10;

pub fn view(state: &State, p: &Palette, frame: &mut Frame) {
    let area = frame.area();
    let [transcript, strip, input, hint] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(input_height(state, area)),
        Constraint::Length(1),
    ])
    .areas(area);
    render_transcript(state, p, frame, transcript);
    render_strip(state, p, frame, strip);
    render_input(state, p, frame, input);
    render_hint(state, p, frame, hint);
    if matches!(state.phase, Phase::WaitingAsk { .. }) {
        render_ask(state, p, frame, transcript);
    }
    if state.help_open {
        render_help(p, frame, transcript);
    }
}

fn render_transcript(state: &State, p: &Palette, frame: &mut Frame, area: Rect) {
    // Wrapping up front (rather than via `Paragraph::wrap`) is what keeps the
    // scroll arithmetic honest: an item that overflows counts as the several
    // rows it really occupies, so Follow still lands on the last one.
    let width = area.width as usize;
    let rows = |i: &TranscriptItem| -> Vec<Line<'static>> {
        item_lines(i, p)
            .iter()
            .flat_map(|l| wrap::line(l, width))
            .collect()
    };
    let lines: Vec<Line> = state.transcript.iter().flat_map(rows).collect();
    let skip = match state.scroll {
        Scroll::Follow => lines.len().saturating_sub(area.height as usize),
        Scroll::At(item) => state.transcript[..item.min(state.transcript.len())]
            .iter()
            .map(|i| rows(i).len())
            .sum::<usize>()
            .min(lines.len().saturating_sub(1)),
    };
    // Slicing beats `Paragraph::scroll`, whose offset is a u16 a long session
    // would overflow.
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(skip)
        .take(area.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible), area);
}

fn item_lines<'a>(item: &TranscriptItem, p: &Palette) -> Vec<Line<'a>> {
    match item {
        TranscriptItem::User { text } => text
            .split('\n')
            .enumerate()
            .map(|(i, l)| {
                let prefix = if i == 0 { "❯ " } else { "  " };
                Line::styled(format!("{prefix}{l}"), Style::new().fg(p.ink).bold())
            })
            .collect(),
        TranscriptItem::Steer { text, queued: true } => vec![Line::styled(
            format!("⤷ {text} — steer queued, applies at next step"),
            Style::new().fg(p.muted),
        )],
        TranscriptItem::Steer {
            text,
            queued: false,
        } => {
            vec![Line::styled(format!("⤷ {text}"), Style::new().fg(p.accent))]
        }
        TranscriptItem::Assistant { text } => text
            .split('\n')
            .map(|l| Line::styled(l.to_string(), Style::new().fg(p.ink)))
            .collect(),
        TranscriptItem::Tool {
            name,
            summary,
            status,
            ticks,
        } => {
            let (marker, color) = match status {
                ToolStatus::Running | ToolStatus::AutoAllowed { .. } => {
                    (SPIN[(*ticks % 4) as usize], p.active)
                }
                ToolStatus::Done => ("✓", p.idle),
                ToolStatus::Failed => ("✗", p.blocked),
                ToolStatus::Denied => ("⛔", p.blocked),
            };
            let (body, mut details) = split_summary(name, summary);
            if let ToolStatus::AutoAllowed { rule } = status {
                details.push(format!("auto-allowed: {rule}"));
            }
            if !matches!(status, ToolStatus::Denied) {
                details.push(format!("{}s", ticks / 8));
            }
            let mut spans = vec![
                Span::styled(format!("  [{marker} {name}]"), Style::new().fg(color)),
                Span::styled(format!(" {body}"), Style::new().fg(p.ink)),
            ];
            if !details.is_empty() {
                spans.push(Span::styled(
                    format!(" · {}", details.join(" · ")),
                    Style::new().fg(p.muted),
                ));
            }
            vec![Line::from(spans)]
        }
        TranscriptItem::Notice { text } => {
            vec![Line::styled(
                format!("  {text}"),
                Style::new().fg(p.muted).italic(),
            )]
        }
    }
}

/// Permission summaries lead with the tool name — "bash [sandboxed:seatbelt]:
/// cargo test", "write ./x". The card already names the tool in its bracket,
/// so peel that prefix off and demote a bracket tag to a muted detail.
fn split_summary(name: &str, summary: &str) -> (String, Vec<String>) {
    let Some(rest) = summary.strip_prefix(name) else {
        return (summary.to_string(), Vec::new());
    };
    if rest.is_empty() {
        return (String::new(), Vec::new());
    }
    if let Some(body) = rest.strip_prefix(':') {
        return (body.trim_start().to_string(), Vec::new());
    }
    if !rest.starts_with(char::is_whitespace) {
        return (summary.to_string(), Vec::new()); // name is a mere prefix, not a word
    }
    let rest = rest.trim_start();
    if let Some((tag, body)) = rest
        .strip_prefix('[')
        .and_then(|tagged| tagged.split_once("]:"))
    {
        return (body.trim_start().to_string(), vec![tag.to_string()]);
    }
    (rest.to_string(), Vec::new())
}

fn render_strip(state: &State, p: &Palette, frame: &mut Frame, area: Rect) {
    // The band background is the watch look; blocked = "waiting on you".
    let style = match state.phase {
        Phase::WaitingAsk { .. } => Style::new().fg(p.blocked).bg(p.band).bold(),
        Phase::Idle => Style::new().fg(p.muted).bg(p.band),
        _ => Style::new().fg(p.ink).bg(p.band),
    };
    frame.render_widget(Paragraph::new(anim::strip_line(state)).style(style), area);

    // Session-name chip, right-aligned on the strip (the Claude-style badge
    // just above the input). The left side stays reserved for the activity
    // glyphs; too-narrow terminals drop the chip rather than collide.
    if let Some(name) = &state.session_name {
        let avail = area.width.saturating_sub(14) as usize;
        if avail >= 8 {
            let mut label: String = name.chars().take(avail - 2).collect();
            if label.chars().count() < name.chars().count() {
                label.pop();
                label.push('…');
            }
            let chip = format!(" {label} ");
            let w = chip.chars().count() as u16;
            let rect = Rect {
                x: area.x + area.width - w,
                y: area.y,
                width: w,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(chip).style(Style::new().fg(p.band).bg(p.accent).bold()),
                rect,
            );
        }
    }
}

/// Every screen row the buffer occupies, plus where the cursor sits among
/// them. Each logical line contributes one row per wrap, so a typed-over-the-
/// edge line continues below instead of running off it, and the cursor rides
/// along instead of pinning to the right margin.
fn input_rows(text: &str, cursor: (usize, usize), width: usize) -> (Vec<String>, (usize, usize)) {
    let mut out: Vec<String> = Vec::new();
    let mut at = (0, 0);
    for (r, line) in text.split('\n').enumerate() {
        let rows = wrap::rows(line, width);
        let last = rows.len() - 1;
        for (i, &(a, b)) in rows.iter().enumerate() {
            // Ranges are contiguous, so exactly one row claims the cursor —
            // the final row also claims the column just past its end.
            if r == cursor.0 && cursor.1 >= a && (cursor.1 < b || i == last) {
                at = (out.len(), wrap::columns(line, a, cursor.1));
            }
            out.push(wrap::slice(line, a, b));
        }
        // A cursor one past a brim-full row belongs at the start of the next
        // one, not a column beyond the border.
        if r == cursor.0 && at.1 >= width {
            out.push(String::new());
            at = (out.len() - 1, 0);
        }
    }
    (out, at)
}

/// The box grows with the wrapped buffer instead of clipping it — bounded so
/// the transcript keeps its 3-row minimum.
fn input_height(state: &State, area: Rect) -> u16 {
    let width = (area.width.saturating_sub(2)).max(1) as usize;
    let (rows, _) = input_rows(&state.editor.text(), state.editor.cursor(), width);
    let body = rows.len().clamp(1, INPUT_MAX_ROWS) as u16;
    (body + 2).min(area.height.saturating_sub(5)).max(3)
}

fn render_input(state: &State, p: &Palette, frame: &mut Frame, area: Rect) {
    let mut block = Block::bordered().border_style(Style::new().fg(p.faint));
    if state.vim_mode {
        let mode = match state.editor.mode() {
            Mode::Insert => "-- INSERT --",
            Mode::Normal => "-- NORMAL --",
        };
        block = block.title(Span::styled(mode, Style::new().fg(p.accent).bold()));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let height = inner.height as usize;
    let (rows, (row, col)) = input_rows(&state.editor.text(), state.editor.cursor(), width);
    // A buffer taller than the box scrolls to keep the cursor's row in view.
    let top = row.saturating_sub(height - 1);
    let lines: Vec<Line> = rows
        .into_iter()
        .skip(top)
        .take(height)
        .map(Line::raw)
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    let x = inner.x + (col as u16).min(inner.width - 1);
    frame.set_cursor_position((x, inner.y + (row - top) as u16));
}

fn render_hint(state: &State, p: &Palette, frame: &mut Frame, area: Rect) {
    let hint = match (&state.phase, state.vim_mode, state.editor.mode()) {
        (Phase::WaitingAsk { .. }, ..) => {
            "y allow · n deny · type a reason after n · ctrl-c cancel"
        }
        (_, true, Mode::Normal) => "i insert · j/k scroll · ctrl-e editor · esc interrupt · ? help",
        _ => "ctrl-e editor · esc interrupt · ? help",
    };
    frame.render_widget(Paragraph::new(hint).style(Style::new().fg(p.faint)), area);
}

fn render_ask(state: &State, p: &Palette, frame: &mut Frame, over: Rect) {
    let Phase::WaitingAsk {
        summary,
        protected_why,
        input,
        denying,
        ..
    } = &state.phase
    else {
        return;
    };
    let mut lines = vec![Line::styled(summary.clone(), Style::new().fg(p.ink).bold())];
    if let Some(why) = protected_why {
        lines.push(Line::styled(
            format!("⚠ {why}"),
            Style::new().fg(p.blocked).bold(),
        ));
    }
    lines.push(Line::raw(""));
    if *denying {
        lines.push(Line::styled(
            format!("deny reason: {input}▏"),
            Style::new().fg(p.ink),
        ));
    } else {
        lines.push(Line::styled(
            "y allow · n deny · type a reason after n",
            Style::new().fg(p.faint),
        ));
    }
    // A long command — or a long deny reason — grows the card downward rather
    // than vanishing off its right edge.
    let lines: Vec<Line> = lines
        .iter()
        .flat_map(|l| wrap::line(l, centered(over, 60, 0).width.saturating_sub(2) as usize))
        .collect();
    let area = centered(over, 60, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" waiting on you ")
        .border_style(Style::new().fg(p.blocked));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_help(p: &Palette, frame: &mut Frame, over: Rect) {
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
    .map(|l| Line::styled(l, Style::new().fg(p.ink)))
    .collect();
    let area = centered(over, 70, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" keys ")
        .border_style(Style::new().fg(p.accent));
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
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::State;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw_buffer(state: &State) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| view(state, &Palette::default(), f))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn draw(state: &State) -> Vec<String> {
        let buffer = draw_buffer(state);
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect()
            })
            .collect()
    }

    // 80×24 layout: transcript rows 0-18, strip 19, input 20-22, hint 23.
    const STRIP: usize = 19;
    const INPUT_TOP: usize = 20;
    const HINT: usize = 23;

    #[test]
    fn idle_layout_shows_resting_glyph_and_hint_row() {
        let rows = draw(&State::new(true, "m".into()));
        assert!(
            rows[STRIP].contains("· ─ ·"),
            "resting glyph: {}",
            rows[STRIP]
        );
        assert!(rows[HINT].contains("? help"), "hint row: {}", rows[HINT]);
        assert!(
            rows[INPUT_TOP].contains("-- INSERT --"),
            "mode title: {}",
            rows[INPUT_TOP]
        );
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
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 16,
        };
        let rows = draw(&s);
        assert!(
            rows[STRIP].contains("bash · 2s"),
            "strip elapsed: {}",
            rows[STRIP]
        );
        assert!(
            rows.iter().any(|r| r.contains("bash] echo hi · 2s")),
            "card elapsed"
        );
    }

    #[test]
    fn split_summary_strips_name_and_lifts_tag() {
        assert_eq!(
            split_summary("bash", "bash [sandboxed:seatbelt]: echo hi"),
            ("echo hi".into(), vec!["sandboxed:seatbelt".to_string()])
        );
        assert_eq!(
            split_summary("write", "write ./x"),
            ("./x".into(), Vec::new())
        );
        assert_eq!(
            split_summary("bash", "bashful thing"),
            ("bashful thing".into(), Vec::new()),
            "name must end at a word boundary"
        );
        assert_eq!(
            split_summary("mcp_ask", "run something: x"),
            ("run something: x".into(), Vec::new()),
            "summaries that don't lead with the name pass through"
        );
    }

    #[test]
    fn tool_card_indents_dedupes_name_and_mutes_details() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::Tool {
            name: "bash".into(),
            summary: "bash [sandboxed:seatbelt]: echo hi".into(),
            status: ToolStatus::Done,
            ticks: 8,
        });
        let rows = draw(&s);
        assert!(
            rows[0].starts_with("  [✓ bash] echo hi · sandboxed:seatbelt · 1s"),
            "indented, deduped card: {}",
            rows[0]
        );
        let buf = draw_buffer(&s);
        let p = Palette::default();
        let col = |needle: &str| rows[0][..rows[0].find(needle).unwrap()].chars().count() as u16;
        assert_eq!(
            buf.cell((col("echo"), 0)).unwrap().style().fg,
            Some(p.ink),
            "command body is primary"
        );
        assert_eq!(
            buf.cell((col("sandboxed"), 0)).unwrap().style().fg,
            Some(p.muted),
            "detail tail is muted"
        );
    }

    #[test]
    fn steer_chip_renders_until_admitted() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::Steer {
            text: "go left".into(),
            queued: true,
        });
        let rows = draw(&s).join("\n");
        assert!(rows.contains("⤷ go left — steer queued"), "pinned chip");
        s.transcript[0] = TranscriptItem::Steer {
            text: "go left".into(),
            queued: false,
        };
        let rows = draw(&s).join("\n");
        assert!(rows.contains("⤷ go left"), "chip stays");
        assert!(
            !rows.contains("steer queued"),
            "queued tag gone once admitted"
        );
    }

    #[test]
    fn strip_wears_band_and_running_tool_marker_is_active() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::Tool {
            name: "bash".into(),
            summary: "echo hi".into(),
            status: ToolStatus::Running,
            ticks: 0,
        });
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 0,
        };
        let buf = draw_buffer(&s);
        let p = Palette::default();
        assert_eq!(
            buf.cell((0, 19)).unwrap().style().bg,
            Some(p.band),
            "strip band bg"
        );
        assert_eq!(
            buf.cell((0, 0)).unwrap().style().fg,
            Some(p.active),
            "tool marker active"
        );
    }

    #[test]
    fn normal_mode_titles_input() {
        let mut s = State::new(true, "m".into());
        s.editor
            .handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let rows = draw(&s);
        assert!(
            rows[INPUT_TOP].contains("-- NORMAL --"),
            "{}",
            rows[INPUT_TOP]
        );
    }

    #[test]
    fn session_name_badge_sits_right_aligned_on_the_strip() {
        let mut s = State::new(true, "m".into());
        s.session_name = Some("rust code review".into());
        let rows = draw(&s);
        assert!(
            rows[STRIP].trim_end().ends_with("rust code review"),
            "badge right-aligned: {:?}",
            rows[STRIP]
        );
        // The resting glyph still renders on the left.
        assert!(
            rows[STRIP].contains("· ─ ·"),
            "strip glyphs: {}",
            rows[STRIP]
        );
    }

    #[test]
    fn long_names_truncate_with_ellipsis_and_absent_names_render_nothing() {
        let mut s = State::new(true, "m".into());
        s.session_name = Some("x".repeat(200));
        let rows = draw(&s);
        assert!(rows[STRIP].contains('…'), "truncated: {}", rows[STRIP]);

        let rows = draw(&State::new(true, "m".into()));
        assert!(!rows[STRIP].contains('…'));
    }

    // ---- overflow: wrapping in the transcript, the input, and the modal ----

    /// Cursor position after a draw — the input's whole job is putting it in
    /// the right place once a line wraps.
    fn draw_cursor(state: &State) -> (u16, u16) {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| view(state, &Palette::default(), f))
            .unwrap();
        let p = terminal.get_cursor_position().unwrap();
        (p.x, p.y)
    }

    /// The input box's rows, borders stripped.
    fn input_body(rows: &[String]) -> Vec<String> {
        rows.iter()
            .filter(|r| r.starts_with('\u{2502}'))
            .map(|r| r.trim_matches('\u{2502}').trim_end().to_string())
            .collect()
    }

    #[test]
    fn input_wraps_an_overlong_line_and_grows_the_box() {
        let mut s = State::new(true, "m".into());
        let long = "abcdefghij".repeat(12); // 120 chars into a 78-col box
        s.editor.set_text(&long);
        let rows = draw(&s);
        let body = input_body(&rows);
        assert_eq!(body.len(), 2, "box grew to two rows: {body:#?}");
        assert_eq!(body.concat(), long, "every typed char survives the wrap");
        // The cursor follows onto the second row instead of pinning to the edge.
        assert_eq!(draw_cursor(&s), (1 + 42, 21), "cursor rides the wrap");
    }

    #[test]
    fn input_renders_every_line_of_a_multiline_buffer() {
        let mut s = State::new(true, "m".into());
        s.editor.set_text("first line\nsecond line\nthird line");
        let body = input_body(&draw(&s));
        assert_eq!(body, ["first line", "second line", "third line"]);
        assert_eq!(draw_cursor(&s), (1 + 10, 21), "cursor on the last line");
    }

    #[test]
    fn a_buffer_taller_than_the_box_scrolls_to_the_cursor() {
        let mut s = State::new(true, "m".into());
        let text: Vec<String> = (0..20).map(|i| format!("line{i}")).collect();
        s.editor.set_text(&text.join("\n"));
        let rows = draw(&s);
        let body = input_body(&rows);
        assert_eq!(body.len(), INPUT_MAX_ROWS, "box stops growing");
        assert_eq!(
            body.last().unwrap(),
            "line19",
            "the cursor's row stays in view: {body:#?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("? help")),
            "the hint row is not pushed off screen"
        );
    }

    #[test]
    fn a_huge_buffer_never_starves_the_transcript() {
        let mut s = State::new(true, "m".into());
        s.transcript
            .push(TranscriptItem::Notice { text: "hi".into() });
        s.editor.set_text(
            &(0..100)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let rows = draw(&s);
        assert!(
            rows[0].contains("hi"),
            "transcript keeps its rows: {rows:#?}"
        );
    }

    #[test]
    fn transcript_wraps_long_output_instead_of_clipping_it() {
        let mut s = State::new(true, "m".into());
        let text = "word ".repeat(40); // 200 chars
        s.transcript
            .push(TranscriptItem::Assistant { text: text.clone() });
        let rows = draw(&s);
        let shown: String = rows[..STRIP]
            .iter()
            .map(|r| r.trim_end())
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(
            shown.replace(' ', ""),
            text.replace(' ', ""),
            "all 200 chars land on wrapped rows"
        );
    }

    #[test]
    fn follow_scroll_counts_wrapped_rows_so_the_tail_stays_visible() {
        let mut s = State::new(true, "m".into());
        for i in 0..10 {
            s.transcript.push(TranscriptItem::Assistant {
                text: format!("{i} {}", "x".repeat(200)),
            });
        }
        s.transcript.push(TranscriptItem::Notice {
            text: "the newest line".into(),
        });
        let rows = draw(&s);
        assert!(
            rows[STRIP - 1].contains("the newest line"),
            "Follow lands on the last wrapped row: {:?}",
            rows[STRIP - 1]
        );
    }

    #[test]
    fn a_long_summary_grows_the_ask_modal_instead_of_overflowing_it() {
        let mut s = State::new(true, "m".into());
        let cmd = "cargo test --workspace --all-features -- --nocapture --test-threads 1";
        s.phase = Phase::WaitingAsk {
            req_id: 7,
            summary: format!("run bash: {cmd}"),
            protected_why: None,
            input: String::new(),
            denying: false,
        };
        let all = draw(&s).join("\n").replace('\n', " ");
        assert!(
            all.contains("--test-threads 1"),
            "the tail of the command is readable: {all}"
        );
    }

    #[test]
    fn wide_glyphs_wrap_on_columns_not_char_counts() {
        let mut s = State::new(true, "m".into());
        s.editor.set_text(&"\u{65e5}".repeat(50)); // 50 chars, 100 columns
        let body = input_body(&draw(&s));
        assert_eq!(body.len(), 2, "78 columns holds 39 wide glyphs: {body:#?}");
        // A wide glyph owns two cells, the second rendered as a blank.
        assert_eq!(body[0].matches('\u{65e5}').count(), 39);
    }
}
