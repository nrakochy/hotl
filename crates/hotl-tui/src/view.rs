//! Pure view: transcript viewport, activity strip, bordered input, hint row,
//! plus the ask modal and help overlay. Renders only from `State` — no clocks,
//! no I/O. Colors come from the shared `hotl_theme::Palette` resolved from
//! `[settings.theme]` — the same palette `hotl watch` wears. Status slots keep
//! watch's semantics: active = working, blocked = needs you, idle = settled.

use hotl_theme::Palette;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::anim;
use crate::app::{DiffOp, Phase, Scroll, State, ToolStatus, TranscriptItem};
use crate::vim::Mode;
use crate::wrap;

const SPIN: [&str; 4] = ["◐", "◓", "◑", "◒"];

/// How tall the input box may grow before it scrolls instead. Past this the
/// buffer is long enough that `ctrl-e` is the better tool anyway.
const INPUT_MAX_ROWS: usize = 10;

/// How many completion rows show at once before the list scrolls. Past this
/// the human should type another character rather than scroll a menu.
const COMPLETE_MAX_ROWS: usize = 8;

/// How much model reasoning shows before it is folded behind `ctrl-t`.
/// Reasoning is context for a decision, not the decision.
const THINKING_COLLAPSED_LINES: usize = 3;

/// The four horizontal bands: transcript, status strip, input, hint. Shared by
/// `view` and `selection_text` so the render and the copy can never disagree
/// about where the transcript ends and the input box begins.
fn regions(state: &State, area: Rect) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(input_height(state, area)),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// The text under a drag selection, read back out of a rendered frame.
///
/// The transcript's spine is trimmed so dragging across a paragraph yields
/// prose; the input box and hint are taken verbatim. Returns empty when the
/// region holds nothing but whitespace.
pub fn selection_text(
    state: &State,
    buf: &ratatui::buffer::Buffer,
    sel: &crate::select::Selection,
) -> String {
    let transcript = regions(state, buf.area)[0];
    // Where `Spine::wrap` hands the line over to content.
    let text_col = state.density.gutter() as u16 + 2;
    crate::select::region_text(buf, sel, transcript, text_col)
}

/// Reverse the selected cells. Runs last of all, so the highlight sits above
/// every widget and popup. Reversed video is what terminals use for their own
/// drag-select, so it reads correctly under any theme and needs no palette
/// entry of its own.
fn highlight(sel: &crate::select::Selection, frame: &mut Frame) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if sel.contains(x, y) {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_style(Style::new().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

pub fn view(state: &State, p: &Palette, frame: &mut Frame) {
    let area = frame.area();
    let [transcript, strip, input, hint] = regions(state, area);
    render_transcript(state, p, frame, transcript);
    render_strip(state, p, frame, strip);
    render_input(state, p, frame, input);
    render_hint(state, p, frame, hint);
    render_completion(state, p, frame, transcript);
    if matches!(state.phase, Phase::WaitingAsk { .. }) {
        render_ask(state, p, frame, transcript);
    }
    if matches!(state.phase, Phase::WaitingQuestion { .. }) {
        render_question(state, p, frame, transcript);
    }
    if matches!(state.phase, Phase::WaitingEgress { .. }) {
        render_egress(state, p, frame, transcript);
    }
    if state.help_open {
        render_help(p, frame, transcript);
    }
    if let Some(sel) = &state.selection {
        highlight(sel, frame);
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
        lines.extend(item_visual_lines(
            item,
            p,
            width,
            gutter,
            state.thinking_expanded,
        ));
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
    thinking_expanded: bool,
) -> Vec<Line<'a>> {
    let (spine, content) = item_block(item, p, thinking_expanded);
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

/// One visual row with well-formed paste/image tokens styled as chips.
/// Grammar-only (`paste::token_ranges` — no side table consulted): a stale
/// token still styles, which is honest — it will also submit literally. A
/// token split across a wrap row renders unstyled; cosmetic only, the
/// submit-time expansion never sees rows. Widths are unchanged, so the
/// cursor math in `input_rows` is untouched.
fn token_line<'a>(row: String, base: Style, token: Style) -> Line<'a> {
    let ranges = crate::paste::token_ranges(&row);
    if ranges.is_empty() {
        return Line::styled(row, base);
    }
    let mut spans = Vec::with_capacity(ranges.len() * 2 + 1);
    let mut at = 0;
    for r in ranges {
        if r.start > at {
            spans.push(Span::styled(row[at..r.start].to_string(), base));
        }
        spans.push(Span::styled(row[r.clone()].to_string(), token));
        at = r.end;
    }
    if at < row.len() {
        spans.push(Span::styled(row[at..].to_string(), base));
    }
    Line::from(spans)
}

/// The spine and the content spans for one item — the content no longer
/// carries its own marker prefix; the spine owns that column now.
fn item_block<'a>(
    item: &TranscriptItem,
    p: &Palette,
    thinking_expanded: bool,
) -> (Spine, Vec<Line<'a>>) {
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
                .map(|l| {
                    token_line(
                        l.to_string(),
                        Style::new().fg(p.ink).bold(),
                        Style::new().fg(p.accent).bold(),
                    )
                })
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
            vec![token_line(
                format!("{text} — steer queued, applies at next step"),
                Style::new().fg(p.muted),
                Style::new().fg(p.accent),
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
            vec![token_line(
                text.to_string(),
                Style::new().fg(p.accent),
                Style::new().fg(p.accent).bold(),
            )],
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
        // Reasoning: dimmed italic behind a faint spine, collapsed by default.
        // The trailer names the toggle so it is discoverable without opening
        // the help overlay.
        TranscriptItem::Thinking { text } => {
            let style = Style::new().fg(p.faint).italic();
            let all: Vec<&str> = text.split('\n').collect();
            let mut lines: Vec<Line> = Vec::new();
            let shown = if thinking_expanded {
                all.len()
            } else {
                THINKING_COLLAPSED_LINES.min(all.len())
            };
            for l in &all[..shown] {
                lines.push(Line::styled(l.to_string(), style));
            }
            if shown < all.len() {
                lines.push(Line::styled(
                    format!("… [+{} lines · ctrl-t]", all.len() - shown),
                    Style::new().fg(p.faint).dim(),
                ));
            }
            (
                Spine {
                    marker: "·",
                    cont: " ",
                    marker_style: Style::new().fg(p.faint),
                    cont_style: Style::new(),
                },
                lines,
            )
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
        Phase::WaitingAsk { .. } | Phase::WaitingQuestion { .. } | Phase::WaitingEgress { .. } => {
            Style::new().fg(p.blocked).bg(p.band).bold()
        }
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

    // Mode badge, just left of the name chip. Always drawn: silence used to
    // mean "ask", but a narrow terminal drops the chip too, so absence was
    // ambiguous — and the mode it implied was wrong (`hotl setup` writes
    // `mode = "bypass"`, evaluation §5.7). A supervision tool states its
    // posture; it does not imply it by omission.
    //
    // Both axes ride one chip (`plan · bypass`): they are independent, and a
    // badge showing only the mode would hide half the posture.
    // INVARIANT: every mode renders its own name. Enforced by
    // `the_mode_badge_is_always_drawn`.
    {
        let chip = if state.plan {
            format!(" plan · {} ", state.mode)
        } else {
            format!(" {} ", state.mode)
        };
        let w = chip.chars().count() as u16;
        if w <= area.width {
            let name_w = state
                .session_name
                .as_ref()
                .map(|n| (n.chars().count() as u16 + 2).min(area.width.saturating_sub(14).max(2)))
                .unwrap_or(0);
            if w + name_w <= area.width {
                // Unattended postures wear the blocked color: nobody is being
                // consulted on this session's tool calls. Plan outranks that —
                // it is the posture the user deliberately chose.
                let style = if state.plan {
                    Style::new().fg(p.band).bg(p.accent).bold()
                } else {
                    match state.mode.as_str() {
                        "bypass" | "dontask" => Style::new().fg(p.band).bg(p.blocked).bold(),
                        _ => Style::new().fg(p.muted).bg(p.band),
                    }
                };
                let rect = Rect {
                    x: area.x + area.width - name_w - w,
                    y: area.y,
                    width: w,
                    height: 1,
                };
                frame.render_widget(Paragraph::new(chip).style(style), rect);
            }
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
        .map(|r| token_line(r, Style::new(), Style::new().fg(p.accent)))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    let x = inner.x + (col as u16).min(inner.width - 1);
    frame.set_cursor_position((x, inner.y + (row - top) as u16));
}

fn render_hint(state: &State, p: &Palette, frame: &mut Frame, area: Rect) {
    // Order is the contract, and it is one-directional: a permission ask or
    // structured question owns the keyboard (`on_ask_key`/`on_question_key`
    // intercept every key at `app::on_key`), so their hints outrank both the
    // completion popup and a live reverse-i-search — a hint naming keys the
    // active handler ignores is worse than no hint at all. Mirrors `6e44471`,
    // which fixed exactly this for the popup; tracker #13 is the sibling.
    // INVARIANT: while `Phase` is `WaitingAsk`/`WaitingQuestion` the hint names
    // only keys that phase's handler accepts. Enforced by
    // `an_ask_during_a_search_shows_the_ask_hint`.
    //
    // The copy notice sits *below* all four of those for the same reason: a
    // mouse drag can finish during an ask, and "copied 3 lines" must not
    // displace the keys the halted loop is waiting on. Enforced by
    // `an_ask_hint_outranks_the_copy_notice`.
    let copied = state.copy_notice.map(|n| {
        let plural = if n == 1 { "" } else { "s" };
        format!("copied {n} line{plural} · any key clears")
    });
    let hint = match (&state.phase, state.vim_mode, state.editor.mode()) {
        (Phase::WaitingAsk { .. }, ..) => {
            "y allow · n deny · type a reason after n · esc interrupt · ctrl-c"
        }
        (Phase::WaitingQuestion { .. }, ..) => {
            "1-9 pick an option · type for free text · enter submit · esc clear/interrupt"
        }
        (Phase::WaitingEgress { .. }, ..) => {
            "y allow this host for the session · n deny · esc interrupt · ctrl-c"
        }
        _ if state.editor.search_prompt().is_some() => {
            "type to search · ctrl-r older · enter accept · esc cancel"
        }
        _ if state.completion.is_some() => "↑↓ pick · tab complete · enter run · esc dismiss",
        _ if copied.is_some() => copied.as_deref().unwrap_or_default(),
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
        diff,
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
    // The proposed change, between the summary and the y/n line — approving a
    // write without seeing it is the gap this closes. Empty until the engine's
    // ask carries the tool input (RQ-2), and an empty diff must render exactly
    // as the card did before.
    if !diff.is_empty() {
        lines.push(Line::raw(""));
        for l in diff {
            let (prefix, style) = match l.op {
                DiffOp::Add => ("+ ", Style::new().fg(p.idle)),
                DiffOp::Del => ("- ", Style::new().fg(p.blocked)),
                DiffOp::Ctx => ("  ", Style::new().fg(p.muted)),
                DiffOp::Trailer => ("  ", Style::new().fg(p.faint).dim()),
            };
            lines.push(Line::styled(format!("{prefix}{}", l.text), style));
        }
    }
    lines.push(Line::raw(""));
    if *denying {
        lines.push(Line::styled(
            format!("deny reason: {input}▏"),
            Style::new().fg(p.ink),
        ));
    } else {
        // Plan 0022: `s` is offered only where it does something — a bash ask
        // whose label does not already say the credential reads are open.
        // An option that is a no-op is worse than no option.
        lines.push(Line::styled(
            if crate::app::secret_read_grant_applies(summary) {
                "y allow · s allow + credential reads (this command only) · n deny"
            } else {
                "y allow · n deny · type a reason after n"
            },
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

/// `ask_user`'s option-picker modal (tier-1 gap #4) — generalizes
/// `render_ask`'s y/n card to N labelled options (2-4, per the tool's own
/// validation) plus free text. Not a permission card: no "waiting on you"
/// urgency color beyond the shared blocked-phase band the strip already
/// carries.
fn render_question(state: &State, p: &Palette, frame: &mut Frame, over: Rect) {
    let Phase::WaitingQuestion {
        header,
        prompt,
        options,
        input,
        ..
    } = &state.phase
    else {
        return;
    };
    let mut lines = vec![
        Line::styled(header.clone(), Style::new().fg(p.ink).bold()),
        Line::styled(prompt.clone(), Style::new().fg(p.ink)),
        Line::raw(""),
    ];
    for (i, opt) in options.iter().enumerate() {
        let mut text = format!("{}) {}", i + 1, opt.label);
        if let Some(desc) = &opt.description {
            text.push_str(&format!(" — {desc}"));
        }
        lines.push(Line::styled(text, Style::new().fg(p.ink)));
    }
    lines.push(Line::raw(""));
    if input.is_empty() {
        lines.push(Line::styled(
            "1-9 pick an option, or type free text",
            Style::new().fg(p.faint),
        ));
    } else {
        lines.push(Line::styled(
            format!("free text: {input}▏"),
            Style::new().fg(p.ink),
        ));
    }
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
        "esc normal mode · esc (empty) interrupt · esc again take control back",
        "i a I A o O insert · h l 0 $ w b e motions (+counts)",
        "d c y operators · dd cc yy x p u",
        "j k scroll transcript when input is empty",
        "pgup pgdn scroll · ctrl-home/end jump · mouse wheel",
        "drag to select and copy · shift-drag for the terminal's own select",
        "ctrl-t expand model thinking",
        "/help /status /cost /clear /quit · /rename /plan /mode /reload",
        "↑ ↓ recall prompt history (prefix-aware) · ctrl-r search history",
        "/ opens command completion · ↑ ↓ pick · tab complete · enter run",
        "ctrl-e or :e open $EDITOR · ctrl-c quit (busy: cancel, again quit)",
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

/// The egress modal (plan 0026). Rendered deliberately unlike `render_ask`:
/// different title, the host on its own line, and a first line that says why
/// this prompt exists at all. A human who just approved `npm install` must not
/// read this as a duplicate of the ask they answered a second ago — under the
/// threat model that reflex is the failure that matters most.
fn render_egress(state: &State, p: &Palette, frame: &mut Frame, over: Rect) {
    let Phase::WaitingEgress { host, .. } = &state.phase else {
        return;
    };
    let lines = [
        Line::styled(
            format!("reaching \"{host}\" was not in the approved command"),
            Style::new().fg(p.ink).bold(),
        ),
        Line::styled(
            "this host is not in [network].allow".to_string(),
            Style::new().fg(p.muted),
        ),
        Line::raw(""),
        Line::styled(
            "y allow for this session · n deny",
            Style::new().fg(p.faint),
        ),
    ];
    let lines: Vec<Line> = lines
        .iter()
        .flat_map(|l| wrap::line(l, centered(over, 60, 0).width.saturating_sub(2) as usize))
        .collect();
    let area = centered(over, 60, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" network egress ")
        .border_style(Style::new().fg(p.blocked));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The `/`-command menu: a bordered list pinned to the bottom-left of the
/// transcript area, so it reads as rising out of the input box rather than
/// floating like the ask/question modals do.
fn render_completion(state: &State, p: &Palette, frame: &mut Frame, over: Rect) {
    // A permission ask / structured question owns the screen; never draw
    // the menu underneath its "waiting on you" card even if `state.completion`
    // were somehow left populated (belt-and-braces alongside the clear in
    // `app::update`).
    if matches!(
        state.phase,
        Phase::WaitingAsk { .. } | Phase::WaitingQuestion { .. } | Phase::WaitingEgress { .. }
    ) {
        return;
    }
    let Some(c) = &state.completion else {
        return;
    };
    // Scroll so the selection stays visible, the same arithmetic the input
    // box uses for a buffer taller than its height.
    let top = c.selected.saturating_sub(COMPLETE_MAX_ROWS - 1);
    let rows: Vec<&crate::complete::Command> = c
        .matches
        .iter()
        .skip(top)
        .take(COMPLETE_MAX_ROWS)
        .filter_map(|&i| state.commands.get(i))
        .collect();
    if rows.is_empty() {
        return;
    }
    let name_w = rows
        .iter()
        .map(|cmd| cmd.name.chars().count())
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let marker = if top + i == c.selected { "› " } else { "  " };
            let mut spans = vec![
                Span::styled(marker, Style::new().fg(p.accent).bold()),
                Span::styled(format!("/{:<name_w$}", cmd.name), Style::new().fg(p.accent)),
            ];
            if !cmd.description.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", cmd.description),
                    Style::new().fg(p.faint),
                ));
            }
            Line::from(spans)
        })
        .collect();
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 2;
    let area = above_input(over, width, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" commands ")
        .border_style(Style::new().fg(p.faint));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // No wrap: an overlong description clips at the border rather than
    // pushing the menu taller than the transcript it sits over.
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A rect `width`×`height`, pinned to `over`'s bottom-left corner.
fn above_input(over: Rect, width: u16, height: u16) -> Rect {
    let width = width.max(10).min(over.width);
    let height = height.min(over.height);
    Rect {
        x: over.x,
        y: over.y + over.height - height,
        width,
        height,
    }
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

    /// Column the `Comfortable` spine (gutter 2 + glyph + space) hands prose
    /// over at, and the transcript text used by the selection tests.
    const TEXT_COL: u16 = 4;
    const PROSE: &str = "alpha beta gamma";

    /// One assistant turn, so transcript row 0 is `"  ● alpha beta gamma"`.
    fn state_with_prose() -> State {
        let mut s = State::test_default();
        s.transcript = vec![TranscriptItem::Assistant {
            text: PROSE.to_string(),
        }];
        s
    }

    /// Every row that has reversed cells, as `(row, text of those cells)`.
    fn reversed_rows(buffer: &ratatui::buffer::Buffer) -> Vec<(u16, String)> {
        (0..buffer.area.height)
            .filter_map(|y| {
                let text: String = (0..buffer.area.width)
                    .filter(|&x| {
                        buffer
                            .cell((x, y))
                            .unwrap()
                            .modifier
                            .contains(Modifier::REVERSED)
                    })
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect();
                (!text.is_empty()).then_some((y, text))
            })
            .collect()
    }

    #[test]
    fn a_drag_highlights_exactly_the_cells_it_covers() {
        let mut s = state_with_prose();
        s.selection = Some(crate::select::Selection {
            anchor: (TEXT_COL, 0),
            head: (TEXT_COL + 4, 0),
        });
        let buffer = draw_buffer(&s);
        assert_eq!(
            reversed_rows(&buffer),
            vec![(0, "alpha".to_string())],
            "only the dragged cells may reverse"
        );
    }

    #[test]
    fn what_is_highlighted_is_what_gets_copied() {
        // The feature's central invariant: the painted region and the scraped
        // text are read from the same buffer, so they cannot disagree.
        let mut s = state_with_prose();
        let sel = crate::select::Selection {
            anchor: (TEXT_COL, 0),
            head: (TEXT_COL + 9, 0),
        };
        s.selection = Some(sel);
        let buffer = draw_buffer(&s);
        let highlighted: String = reversed_rows(&buffer)
            .into_iter()
            .map(|(_, text)| text.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(highlighted, selection_text(&s, &buffer, &sel));
        assert_eq!(highlighted, "alpha beta");
    }

    #[test]
    fn dragging_from_the_left_edge_copies_prose_without_the_spine() {
        let mut s = state_with_prose();
        let sel = crate::select::Selection {
            anchor: (0, 0),
            head: (79, 0),
        };
        s.selection = Some(sel);
        assert_eq!(selection_text(&s, &draw_buffer(&s), &sel), PROSE);
    }

    #[test]
    fn a_finished_copy_is_reported_in_the_hint() {
        let mut s = State::test_default();
        s.copy_notice = Some(3);
        assert!(draw(&s)[HINT].contains("copied 3 lines"), "{:?}", draw(&s));
    }

    #[test]
    fn one_copied_line_reads_in_the_singular() {
        let mut s = State::test_default();
        s.copy_notice = Some(1);
        assert!(draw(&s)[HINT].contains("copied 1 line ·"), "{:?}", draw(&s));
    }

    #[test]
    fn the_help_overlay_names_drag_to_copy() {
        let mut s = State::test_default();
        s.help_open = true;
        let rows = draw(&s);
        assert!(
            rows.iter().any(|r| r.contains("drag to select and copy")),
            "{rows:?}"
        );
    }

    #[test]
    fn an_ask_hint_outranks_the_copy_notice() {
        // The hint-precedence INVARIANT: a phase that owns the keyboard must
        // keep naming its own keys.
        let mut s = State::test_default();
        s.copy_notice = Some(3);
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "run rm -rf".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        assert!(draw(&s)[HINT].contains("y allow"), "{:?}", draw(&s));
    }

    /// A human who just approved `npm install` must not read the egress modal
    /// as a duplicate of the ask they answered a second ago. Snapshot-shaped
    /// so a later refactor cannot quietly merge the two renderings.
    #[test]
    fn the_egress_modal_is_visually_distinct_from_a_tool_ask() {
        let mut ask = State::test_default();
        ask.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "bash: npm install".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        let ask_out = draw(&ask).join("\n");

        let mut egress = State::test_default();
        egress.phase = Phase::WaitingEgress {
            req_id: 2,
            host: "registry.npmjs.org".into(),
        };
        let out = draw(&egress).join("\n");

        assert!(out.contains("network egress"), "own title: {out}");
        assert!(!ask_out.contains("network egress"));
        // Wrapped across the card's 60 columns, so match the tail of the
        // sentence rather than the whole of it.
        assert!(
            out.contains("approved command"),
            "the reason this prompt exists at all: {out}"
        );
        assert!(out.contains("registry.npmjs.org"), "{out}");
        assert!(out.contains("[network].allow"), "name the control: {out}");
        // The tool ask's card title, which this card must not borrow. (The
        // activity strip still reads "waiting on you · network" — that is the
        // strip, not the card.)
        assert!(ask_out.contains("┌ waiting on you"), "{ask_out}");
        assert!(
            !out.contains("┌ waiting on you"),
            "the tool ask's card title must not appear on the egress card: {out}"
        );
        assert!(
            draw(&egress)[HINT].contains("allow this host for the session"),
            "{:?}",
            draw(&egress)[HINT]
        );
    }

    #[test]
    fn thinking_collapses_to_three_lines_with_a_toggle_hint() {
        let mut s = State::test_default();
        s.transcript = vec![TranscriptItem::Thinking {
            text: (1..=6)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }];
        let out = draw(&s).join("\n");
        assert!(out.contains("line3") && !out.contains("line4"), "{out}");
        assert!(out.contains("ctrl-t"), "the toggle must be named: {out}");

        s.thinking_expanded = true;
        let out = draw(&s).join("\n");
        assert!(out.contains("line6"), "{out}");
    }

    #[test]
    fn the_ask_card_renders_a_diff_when_one_is_supplied() {
        use crate::app::DiffLine;
        let mut s = State::test_default();
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "edit ./x.rs".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: vec![
                DiffLine {
                    op: DiffOp::Ctx,
                    text: "fn main() {".into(),
                },
                DiffLine {
                    op: DiffOp::Del,
                    text: "    old();".into(),
                },
                DiffLine {
                    op: DiffOp::Add,
                    text: "    new();".into(),
                },
            ],
        };
        let out = draw(&s).join("\n");
        assert!(out.contains("- ") && out.contains("old();"), "{out}");
        assert!(out.contains("+ ") && out.contains("new();"), "{out}");
    }

    /// Until R2 lands RQ-2 every ask arrives without a diff; that path must
    /// stay exactly the card it was.
    #[test]
    fn an_ask_with_no_diff_renders_exactly_as_before() {
        let mut s = State::test_default();
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "write ./x".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        let rows = draw(&s);
        assert!(rows.join("\n").contains("write ./x"));
        // Scoped to the card: the input box's own "-- INSERT --" title would
        // otherwise trip a whole-screen search for a `- ` prefix.
        for row in card_rows(&rows) {
            let body = row.trim();
            assert!(
                !body.starts_with("+ ") && !body.starts_with("- "),
                "diff row in a diffless card: {row:?}"
            );
        }
    }

    /// The interior rows of the bordered ask card.
    fn card_rows(rows: &[String]) -> Vec<String> {
        let edges: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.contains('┌') || r.contains('└'))
            .map(|(i, _)| i)
            .collect();
        let (&top, &bottom) = (
            edges.first().expect("card top"),
            edges.last().expect("card bottom"),
        );
        rows[top + 1..bottom]
            .iter()
            .map(|r| r.replace(['│', '┃'], " "))
            .collect()
    }

    /// Tracker #13. A permission ask owns the keyboard; the hint must name the
    /// keys `on_ask_key` actually handles, not the four a live Ctrl-R
    /// advertises — all of which that handler ignores.
    #[test]
    fn an_ask_during_a_search_shows_the_ask_hint() {
        let mut s = State::test_default();
        s.editor.load_history(vec!["cargo test".into()]);
        s.editor
            .handle(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "write ./x".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        let hint = draw(&s)[HINT].clone();
        assert!(hint.contains("y allow"), "got: {hint}");
        assert!(
            !hint.contains("ctrl-r older"),
            "dead keys advertised: {hint}"
        );
    }

    /// §5.7 bug 3: the badge used to be silent on "ask" while the shipped
    /// default was "auto" — so "no badge" could mean either that or a terminal
    /// too narrow for the chip. Every mode is now stated outright.
    #[test]
    fn the_mode_badge_is_always_drawn() {
        for mode in ["ask", "auto", "plan", "dontask"] {
            let mut s = State::test_default();
            s.mode = mode.into();
            let rendered = draw(&s).join("\n");
            assert!(rendered.contains(mode), "mode `{mode}` is not on screen");
        }
    }

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
    fn the_help_overlay_documents_the_completion_keys() {
        let mut s = State::new(true, "m".into());
        s.help_open = true;
        let rows = draw(&s);
        assert!(
            rows.iter().any(|r| r.contains("complete")),
            "help must name the / completion keys: {rows:#?}"
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
    fn strip_renders_todo_progress_and_the_active_items_text() {
        let mut s = State::new(true, "m".into());
        s.todos = vec![
            hotl_tools::todo::Todo {
                content: "done thing".into(),
                status: hotl_tools::todo::TodoStatus::Completed,
                active_form: None,
            },
            hotl_tools::todo::Todo {
                content: "wire the gate".into(),
                status: hotl_tools::todo::TodoStatus::InProgress,
                active_form: Some("wiring the gate".into()),
            },
        ];
        let rows = draw(&s);
        assert!(rows[STRIP].contains("1/2"), "progress: {}", rows[STRIP]);
        assert!(
            rows[STRIP].contains("wiring the gate"),
            "active item text: {}",
            rows[STRIP]
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
            diff: Vec::new(),
        };
        let rows = draw(&s);
        let all = rows.join("\n");
        assert!(all.contains("run bash: rm -rf ./x"), "summary in modal");
        assert!(all.contains("⚠ protected path"), "loud protected line");
        assert!(rows[STRIP].contains("waiting on you"), "halted strip");
    }

    #[test]
    fn waiting_question_renders_the_modal_with_numbered_options() {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::WaitingQuestion {
            req_id: 9,
            header: "Scope".into(),
            prompt: "How far?".into(),
            options: vec![
                hotl_tools::ask::QuestionOption {
                    label: "MVP".into(),
                    description: None,
                },
                hotl_tools::ask::QuestionOption {
                    label: "Full".into(),
                    description: Some("everything".into()),
                },
            ],
            input: String::new(),
        };
        let rows = draw(&s);
        let all = rows.join("\n");
        assert!(all.contains("Scope"), "header in modal");
        assert!(all.contains("How far?"), "prompt in modal");
        assert!(all.contains("1) MVP"), "numbered option: {all}");
        assert!(
            all.contains("2) Full — everything"),
            "description shown: {all}"
        );
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
    fn the_mode_badge_lands_on_the_strip() {
        let mut s = State::new(true, "m".into());
        s.mode = "plan".into();
        let rows = draw(&s);
        assert!(
            rows[STRIP].to_lowercase().contains("plan"),
            "plan badge: {:?}",
            rows[STRIP]
        );

        // `ask` used to render nothing, on the reasoning that it is the
        // default posture. That only held if the value were true, and §5.7
        // found it was not — `hotl setup` writes `mode = "bypass"`. It is
        // stated now, on the same row.
        let s = State::new(true, "m".into());
        assert_eq!(s.mode, "ask");
        let rows = draw(&s);
        assert!(
            rows[STRIP].to_lowercase().contains("ask"),
            "ask must badge too: {:?}",
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
            diff: Vec::new(),
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

    // ---- the `/`-command completion popup ----

    fn with_popup() -> State {
        let mut s = State::new(true, "m".into());
        s.commands.push(crate::complete::Command {
            name: "review".into(),
            description: "review a pull request".into(),
            builtin: false,
        });
        s.commands.push(crate::complete::Command {
            name: "bare".into(),
            description: String::new(),
            builtin: false,
        });
        for c in "/re".chars() {
            crate::app::update(
                &mut s,
                crate::app::Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        s
    }

    #[test]
    fn the_completion_popup_sits_above_the_input_with_the_selection_marked() {
        let s = with_popup();
        let rows = draw(&s);
        let popup: Vec<&String> = rows[..INPUT_TOP]
            .iter()
            .filter(|r| r.contains("/reload") || r.contains("/rename") || r.contains("/review"))
            .collect();
        assert_eq!(popup.len(), 3, "every match renders: {rows:#?}");
        assert!(
            popup[0].contains("› /reload"),
            "the first match is marked: {}",
            popup[0]
        );
        assert!(
            popup[0].contains("re-read config.toml"),
            "descriptions render: {}",
            popup[0]
        );
        // 80×24: transcript is rows 0-18, strip 19, input 20-22. Anchored to
        // the transcript's bottom, the popup's lower border lands on row 18 —
        // directly above the strip, not on it.
        assert!(
            rows[STRIP - 1].contains("─"),
            "the popup's bottom border sits on the transcript's last row: {}",
            rows[STRIP - 1]
        );
    }

    /// Finding 1 (blocking): a stale popup must never outrank a permission
    /// ask. This drives `state.completion` directly (rather than through
    /// `app::update`, which already clears it) so the check is independent
    /// of that other guard — the render layer must hold the line on its own.
    #[test]
    fn a_permission_ask_hides_a_stale_popup_and_wins_the_hint_row() {
        let mut s = with_popup();
        assert!(s.completion.is_some(), "popup open before the ask arrives");
        s.phase = Phase::WaitingAsk {
            req_id: 7,
            summary: "run bash: rm -rf ./x".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        let rows = draw(&s);
        assert!(
            rows[HINT].contains("y allow · n deny"),
            "the ask's hint must win over the popup's: {}",
            rows[HINT]
        );
        assert!(
            !rows.iter().any(|r| r.contains("commands")),
            "no popup chrome may render over the ask: {rows:#?}"
        );
    }

    #[test]
    fn a_description_less_command_renders_as_name_only() {
        let mut s = State::new(true, "m".into());
        s.commands.push(crate::complete::Command {
            name: "bare".into(),
            description: String::new(),
            builtin: false,
        });
        for c in "/bar".chars() {
            crate::app::update(
                &mut s,
                crate::app::Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        let rows = draw(&s);
        let row = rows[..INPUT_TOP]
            .iter()
            .find(|r| r.contains("/bare"))
            .expect("the match renders");
        // `Block::bordered` adds no padding, so content butts against the
        // left border: `│› /bare` and nothing after the name.
        assert_eq!(row.trim_end().trim_end_matches('│').trim_end(), "│› /bare");
    }

    #[test]
    fn the_hint_row_names_the_popup_keys_while_it_is_open() {
        let rows = draw(&with_popup());
        assert!(
            rows[HINT].contains("tab complete") && rows[HINT].contains("esc dismiss"),
            "hint row: {}",
            rows[HINT]
        );
    }
}
