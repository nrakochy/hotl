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
    // rows it really occupies, so Follow still lands on the last one. Building
    // the whole buffer in a loop (not a flat_map) lets a blank-line separator
    // sit *between* turns and records where each item starts for At-scroll.
    let width = area.width as usize;
    let gutter = state.density.gutter();
    let blanks = state.density.blank_lines();
    let mut lines: Vec<Line> = Vec::new();
    let mut item_starts: Vec<usize> = Vec::with_capacity(state.transcript.len());
    for (i, item) in state.transcript.iter().enumerate() {
        if i > 0 {
            for _ in 0..blanks {
                lines.push(Line::raw(""));
            }
        }
        item_starts.push(lines.len());
        lines.extend(item_visual_lines(item, p, width, gutter));
    }
    let total = lines.len();
    let skip = match state.scroll {
        Scroll::Follow => total.saturating_sub(area.height as usize),
        Scroll::At(item) => item_starts
            .get(item)
            .copied()
            .unwrap_or(total)
            .min(total.saturating_sub(1)),
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

/// The left-column signature of one turn: a marker glyph on the first visual
/// row, a continuation glyph on the rest, each with its own color. This is
/// what lets the eye track who is speaking by scanning straight down.
struct Spine {
    marker: &'static str,
    cont: &'static str,
    marker_style: Style,
    cont_style: Style,
}

impl Spine {
    /// Prepend the gutter pad and this row's spine glyph to a content line.
    /// The glyph occupies one column; a trailing space separates it from the
    /// text, so content always starts at `gutter + 2`.
    fn wrap<'a>(&self, mut content: Line<'a>, gutter: usize, first: bool) -> Line<'a> {
        let (glyph, style) = if first {
            (self.marker, self.marker_style)
        } else {
            (self.cont, self.cont_style)
        };
        let lead = format!("{}{glyph} ", " ".repeat(gutter));
        let mut spans = Vec::with_capacity(content.spans.len() + 1);
        spans.push(Span::styled(lead, style));
        spans.append(&mut content.spans);
        // Carry the content line's own style through — for lines built with
        // `Line::styled` (plain ink prose, a bold heading, a band-backed code
        // line) the color lives at line level, not on the spans, and dropping
        // it here would render them in the terminal's default style.
        Line {
            spans,
            style: content.style,
            alignment: content.alignment,
        }
    }
}

/// One transcript item as it lands on screen: content wrapped to the width the
/// gutter+spine leave, each row carrying its spine glyph. Used by both the
/// render and the scroll math, so they can never disagree on row counts.
fn item_visual_lines<'a>(
    item: &TranscriptItem,
    p: &Palette,
    width: usize,
    gutter: usize,
) -> Vec<Line<'a>> {
    let (spine, content) = item_block(item, p);
    // `gutter + 2` = the pad plus the one-column glyph and its trailing space.
    let inner = width.saturating_sub(gutter + 2).max(1);
    let mut out = Vec::new();
    for cl in &content {
        for wl in wrap::line(cl, inner) {
            let first = out.is_empty();
            out.push(spine.wrap(wl, gutter, first));
        }
    }
    if out.is_empty() {
        out.push(spine.wrap(Line::raw(""), gutter, true));
    }
    out
}

/// Assistant prose with light, line-level structure so an answer is scannable
/// on its own, not just at the turn boundary. Deliberately NOT a markdown
/// engine: each line is classified by how it begins, nothing spans lines
/// except the fenced-code toggle. Anything unrecognized stays plain ink, so a
/// stray `#` mid-sentence never turns into a heading.
fn assistant_lines<'a>(text: &str, p: &Palette) -> Vec<Line<'a>> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for raw in text.split('\n') {
        let lead = raw.trim_start();
        // ``` toggles a code fence; the fence line itself renders as a quiet
        // divider rather than literal backticks shouting on screen.
        if lead.starts_with("```") {
            in_fence = !in_fence;
            out.push(Line::styled(
                raw.to_string(),
                Style::new().fg(p.faint).dim(),
            ));
            continue;
        }
        if in_fence {
            out.push(code_line(raw, p));
            continue;
        }
        // `#`..`###`-led heading → bold, hashes stripped.
        if let Some(h) = heading_text(lead) {
            out.push(Line::styled(h, Style::new().fg(p.ink).bold()));
            continue;
        }
        // `- ` / `* ` bullet → a `•` marker in the accent, indentation kept.
        if let Some((indent, rest)) = bullet(raw) {
            out.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled("• ", Style::new().fg(p.accent)),
                Span::styled(rest.to_string(), Style::new().fg(p.ink)),
            ]));
            continue;
        }
        // A 4-space indent is markdown's other code form.
        if raw.starts_with("    ") && !raw.trim().is_empty() {
            out.push(code_line(raw, p));
            continue;
        }
        out.push(Line::styled(raw.to_string(), Style::new().fg(p.ink)));
    }
    out
}

/// A code line: muted on the band, so it reads as code without a full-width
/// fill that would fight the gutter and wrapping (the band rides the text).
fn code_line<'a>(raw: &str, p: &Palette) -> Line<'a> {
    Line::styled(raw.to_string(), Style::new().fg(p.muted).bg(p.band))
}

/// The text of a `#`/`##`/`###`(…) heading with the hashes and one space
/// stripped, or `None` if the line is not a heading. Requires a space (or end)
/// after the hashes, so `#42` in prose is not mistaken for one.
fn heading_text(lead: &str) -> Option<String> {
    let rest = lead.trim_start_matches('#');
    let hashes = lead.len() - rest.len();
    if hashes == 0 {
        return None;
    }
    match rest.strip_prefix(' ') {
        Some(body) => Some(body.to_string()),
        None if rest.is_empty() => Some(String::new()),
        None => None, // `#foo` — a hash-word, not a heading
    }
}

/// `(leading_indent, item_text)` for a `- ` or `* ` bullet, else `None`.
fn bullet(raw: &str) -> Option<(&str, &str)> {
    let indent = &raw[..raw.len() - raw.trim_start().len()];
    let lead = &raw[indent.len()..];
    for marker in ["- ", "* "] {
        if let Some(rest) = lead.strip_prefix(marker) {
            return Some((indent, rest));
        }
    }
    None
}

/// The spine and the content spans for one item — the content no longer
/// carries its own marker prefix; the spine owns that column now.
fn item_block<'a>(item: &TranscriptItem, p: &Palette) -> (Spine, Vec<Line<'a>>) {
    match item {
        TranscriptItem::User { text } => (
            // You are the anchor: high-contrast caret, no continuation bar.
            Spine {
                marker: "❯",
                cont: " ",
                marker_style: Style::new().fg(p.ink).bold(),
                cont_style: Style::new(),
            },
            text.split('\n')
                .map(|l| Line::styled(l.to_string(), Style::new().fg(p.ink).bold()))
                .collect(),
        ),
        TranscriptItem::Assistant { text } => (
            // The warm dot + a faint bar down the whole answer, so a long
            // reply reads as one block rather than a wall of flat text.
            Spine {
                marker: "●",
                cont: "│",
                marker_style: Style::new().fg(p.accent),
                cont_style: Style::new().fg(p.faint),
            },
            assistant_lines(text, p),
        ),
        TranscriptItem::Steer { text, queued: true } => (
            Spine {
                marker: "⤷",
                cont: " ",
                marker_style: Style::new().fg(p.muted),
                cont_style: Style::new(),
            },
            vec![Line::styled(
                format!("{text} — steer queued, applies at next step"),
                Style::new().fg(p.muted),
            )],
        ),
        TranscriptItem::Steer {
            text,
            queued: false,
        } => (
            Spine {
                marker: "⤷",
                cont: " ",
                marker_style: Style::new().fg(p.accent),
                cont_style: Style::new(),
            },
            vec![Line::styled(text.to_string(), Style::new().fg(p.accent))],
        ),
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
            // Name in the status color (so it stays identifiable now the
            // marker moved to the spine), body ink, details muted.
            let mut spans = vec![Span::styled(name.clone(), Style::new().fg(color))];
            if !body.is_empty() {
                spans.push(Span::styled(format!("  {body}"), Style::new().fg(p.ink)));
            }
            if !details.is_empty() {
                spans.push(Span::styled(
                    format!(" · {}", details.join(" · ")),
                    Style::new().fg(p.muted),
                ));
            }
            (
                Spine {
                    marker,
                    cont: " ",
                    marker_style: Style::new().fg(color),
                    cont_style: Style::new(),
                },
                vec![Line::from(spans)],
            )
        }
        TranscriptItem::Notice { text } => (
            Spine {
                marker: "·",
                cont: " ",
                marker_style: Style::new().fg(p.muted),
                cont_style: Style::new(),
            },
            vec![Line::styled(
                text.to_string(),
                Style::new().fg(p.muted).italic(),
            )],
        ),
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
    // A live reverse-i-search replaces the buffer view with its prompt line;
    // the cursor rides just after the query.
    if let Some((query, matched)) = state.editor.search_prompt() {
        let head = format!("(reverse-i-search)'{query}': ");
        frame.render_widget(Paragraph::new(format!("{head}{matched}")), inner);
        let col = (head.chars().count() as u16).min(inner.width.saturating_sub(1));
        frame.set_cursor_position((inner.x + col, inner.y));
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
    if state.editor.search_prompt().is_some() {
        let hint = "type to search · ctrl-r older · enter accept · esc cancel";
        frame.render_widget(Paragraph::new(hint).style(Style::new().fg(p.faint)), area);
        return;
    }
    let hint = match (&state.phase, state.vim_mode, state.editor.mode()) {
        (Phase::WaitingAsk { .. }, ..) => {
            "y allow · n deny · type a reason after n · ctrl-c cancel"
        }
        (_, true, Mode::Normal) => "i insert · j/k scroll · ctrl-e editor · esc interrupt · ? help",
        _ => "↑↓ history · ctrl-r search · ctrl-e editor · esc interrupt · ? help",
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
        "↑ ↓ recall prompt history (prefix-aware) · ctrl-r search history",
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
    fn reverse_i_search_prompt_takes_over_the_input_area() {
        let mut s = State::new(true, "m".into());
        s.editor
            .load_history(vec!["deploy staging".into(), "deploy prod".into()]);
        s.editor
            .handle(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        for c in "deploy".chars() {
            s.editor
                .handle(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let all = draw(&s).join("\n");
        assert!(all.contains("reverse-i-search"), "search prompt: {all}");
        assert!(all.contains("'deploy'"), "query echoed: {all}");
        assert!(all.contains("deploy prod"), "newest match shown: {all}");
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
            rows.iter().any(|r| r.contains("bash  echo hi · 2s")),
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
        // Comfortable gutter (2) + the ✓ spine glyph; the name is no longer
        // bracketed, and the duplicate leading "bash" is peeled off the body.
        assert!(
            rows[0].starts_with("  ✓ bash  echo hi · sandboxed:seatbelt · 1s"),
            "spine card: {}",
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
        // Strip the spine glyphs (● first row, │ continuation) and spaces —
        // what remains must be every content char, nothing clipped.
        assert_eq!(
            shown.replace([' ', '●', '│'], ""),
            text.replace(' ', ""),
            "all 200 chars land on wrapped rows"
        );
    }

    #[test]
    fn assistant_turn_shows_marker_then_continuation_bar() {
        let mut s = State::new(true, "m".into());
        s.transcript
            .push(TranscriptItem::User { text: "hi".into() });
        s.transcript.push(TranscriptItem::Assistant {
            text: "line one\nline two".into(),
        });
        let rows = draw(&s);
        // Comfortable gutter = 2. You get the caret; the assistant gets a dot
        // on its first line and a bar on the next.
        assert!(rows[0].starts_with("  ❯ hi"), "user caret: {:?}", rows[0]);
        // A blank line separates the turns (comfortable = 1).
        assert_eq!(rows[1].trim(), "", "blank between turns: {:?}", rows[1]);
        assert!(rows[2].starts_with("  ● line one"), "marker: {:?}", rows[2]);
        assert!(
            rows[3].starts_with("  │ line two"),
            "cont bar: {:?}",
            rows[3]
        );
    }

    #[test]
    fn compact_density_drops_the_blank_and_the_gutter() {
        let mut s = State::new(true, "m".into());
        s.density = hotl_theme::Density::Compact;
        s.transcript
            .push(TranscriptItem::User { text: "hi".into() });
        s.transcript
            .push(TranscriptItem::Assistant { text: "yo".into() });
        let rows = draw(&s);
        // No gutter, no blank line between turns — the dense look, but the
        // spine glyph still marks who is speaking.
        assert!(rows[0].starts_with("❯ hi"), "no gutter: {:?}", rows[0]);
        assert!(rows[1].starts_with("● yo"), "back-to-back: {:?}", rows[1]);
    }

    /// Pull the fg/attrs of the first content cell of a row (past the gutter
    /// and spine) so prose styling can be asserted, not just the glyphs.
    fn cell_fg(state: &State, row: u16, col: u16) -> Option<Color> {
        draw_buffer(state).cell((col, row)).unwrap().style().fg
    }

    #[test]
    fn assistant_prose_styles_headings_bullets_and_code() {
        let mut s = State::new(true, "m".into());
        s.density = hotl_theme::Density::Compact; // gutter 0 → content at col 2
        s.transcript.push(TranscriptItem::Assistant {
            text: "# Setup\n- clone the repo\n```\ncargo build\n```\nplain tail".into(),
        });
        let rows = draw(&s);
        let p = Palette::default();

        // Heading: hashes stripped, bold.
        assert!(
            rows[0].starts_with("● Setup"),
            "heading text: {:?}",
            rows[0]
        );
        assert!(
            draw_buffer(&s)
                .cell((2, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "heading is bold"
        );
        // Bullet: • marker in accent, at the content column.
        assert!(
            rows[1].contains("• clone the repo"),
            "bullet: {:?}",
            rows[1]
        );
        assert_eq!(cell_fg(&s, 1, 2), Some(p.accent), "bullet marker is accent");
        // Fenced code: the code line is muted on the band.
        let code_row = rows.iter().position(|r| r.contains("cargo build")).unwrap();
        let col = rows[code_row].find("cargo").unwrap() as u16;
        let cell = draw_buffer(&s)
            .cell((col, code_row as u16))
            .unwrap()
            .style();
        assert_eq!(cell.fg, Some(p.muted), "code fg muted");
        assert_eq!(cell.bg, Some(p.band), "code on the band");
        // Plain line after the closing fence is back to ink, not on the band
        // (buffer cells default to Reset bg, so assert it's not the band).
        let tail = rows.iter().position(|r| r.contains("plain tail")).unwrap();
        let tcol = rows[tail].find("plain").unwrap() as u16;
        assert_ne!(
            draw_buffer(&s)
                .cell((tcol, tail as u16))
                .unwrap()
                .style()
                .bg,
            Some(p.band),
            "fence closed: tail is not code"
        );
    }

    #[test]
    fn a_hash_word_is_not_a_heading_and_an_open_fence_runs_to_the_end() {
        assert_eq!(heading_text("#42 is a count"), None);
        assert_eq!(heading_text("## Real"), Some("Real".into()));
        assert_eq!(heading_text("plain"), None);
        assert_eq!(bullet("  - nested"), Some(("  ", "nested")));
        assert_eq!(bullet("not a bullet"), None);

        // An unclosed fence keeps everything after it as code. `code_line`
        // carries the band at line level, so check there.
        let p = Palette::default();
        let lines = assistant_lines("```\nline in code\nstill code", &p);
        // [fence marker, code, code]
        assert_eq!(lines[1].style.bg, Some(p.band));
        assert_eq!(lines[2].style.bg, Some(p.band));
    }

    #[test]
    fn follow_scroll_lands_on_the_last_line_with_spacing() {
        // Enough turns to overflow the 19-row transcript, so Follow has to
        // account for the blank separators too.
        let mut s = State::new(true, "m".into());
        for i in 0..30 {
            s.transcript.push(TranscriptItem::Assistant {
                text: format!("answer {i}"),
            });
        }
        let rows = draw(&s);
        assert!(
            rows[..STRIP].iter().any(|r| r.contains("answer 29")),
            "last turn is visible under Follow"
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
