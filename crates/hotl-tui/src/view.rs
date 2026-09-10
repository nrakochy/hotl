//! Pure view: transcript viewport, activity strip, bordered input, hint row,
//! plus the ask modal and help overlay. Renders only from `State` — no clocks,
//! no I/O. Colors come from the shared `hotl_theme::Palette` resolved from
//! `[settings.theme]` — the same palette `hotl watch` wears. Status slots keep
//! watch's semantics: active = working, blocked = needs you, idle = settled.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use hotl_theme::{Density, Palette};
use hotl_types::ContextKind;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::anim;
use crate::app::{
    BandRow, ContextReport, DiffOp, Phase, Scroll, State, ToolStatus, TranscriptItem,
};
use crate::vim::Mode;
use crate::wrap;

/// A settled, successful card's glyph. Deliberately not a tick: success is
/// the common case and should read as a quiet hand-off to the next thing.
const SETTLED_OK_GLYPH: &str = "→";

// Twin of watch-tui's WORKING_FRAMES — keep in sync.
const WORKING_FRAMES: [&str; 16] = [
    "⠑", "⠔", "⣄", "⣠", "⡠", "⠢", "⠚", "⠜", "⠔", "⠤", "⣠", "⣄", "⢄", "⠆", "⠃", "⠑",
];

/// How fast the running-tool card's wanderer steps, in frames per second.
///
/// Deliberately *not* `anim::TICK_HZ`: 8/sec matches the cadence of watch's
/// twin (125 ms), and it is what keeps a running card from invalidating its
/// cached rows on every frame.
const MARKER_HZ: u64 = 8;

/// Which marker frame a card at `ticks` shows.
fn marker_frame(ticks: u64) -> usize {
    (ticks * MARKER_HZ / anim::TICK_HZ) as usize % WORKING_FRAMES.len()
}

/// The draft body's cap: a third of the terminal, never under 5 rows. A
/// tall terminal earns a taller editor; a short one keeps its transcript.
/// Past this the buffer scrolls — and `ctrl-g` is the better tool anyway.
fn draft_cap(area: Rect) -> usize {
    (area.height as usize / 3).max(5)
}

/// How many completion rows show at once before the list scrolls. Past this
/// the human should type another character rather than scroll a menu.
const COMPLETE_MAX_ROWS: usize = 8;

/// How much model reasoning shows before it is folded behind `ctrl-t`.
/// Reasoning is context for a decision, not the decision.
const THINKING_COLLAPSED_LINES: usize = 3;

/// The six horizontal bands: transcript, status strip, gap, input, agent
/// selector, hint. Shared by `view` and `selection_text` so the render and
/// the copy can never disagree about where the transcript ends and the input
/// box begins. The selector band is height 0 with no spawn cards (0039) and
/// the gap row exists only from 30 rows up (0049 T3) — zero-height keeps
/// every pre-existing 80×24 row-indexed view test honest.
///
/// The input band is inset by the gutter, so the box sits on the grid: its
/// left edge at the spine column, its text at the transcript's text column.
fn regions(state: &State, area: Rect) -> [Rect; 6] {
    let [transcript, strip, gap, mut input, selector, hint] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(u16::from(area.height >= 30)),
        Constraint::Length(input_height(state, area)),
        Constraint::Length(selector_height(state)),
        Constraint::Length(1),
    ])
    .areas(area);
    let g = state.density.gutter() as u16;
    input.x += g;
    input.width = input.width.saturating_sub(g);
    [transcript, strip, gap, input, selector, hint]
}

/// The selector's spawn-row window (0039); more spawns overflow to a count.
const SELECTOR_MAX_SPAWNS: usize = 4;

/// 0 with no spawn cards, else the `main` row + up to `SELECTOR_MAX_SPAWNS`
/// spawn rows + one overflow line.
fn selector_height(state: &State) -> u16 {
    let spawns = state.band_spawns().len();
    if spawns == 0 {
        return 0;
    }
    let overflow = usize::from(spawns > SELECTOR_MAX_SPAWNS);
    (1 + spawns.min(SELECTOR_MAX_SPAWNS) + overflow) as u16
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

/// Wrapped transcript rows, memoized per item across frames.
///
/// Wrapping the session is by far the most expensive thing the view does, and
/// the animation ticks at `anim::TICK_HZ` — without a cache, a moving wave
/// would re-wrap and re-allocate every row of a long session 30 times a
/// second.
///
/// The memo is **per item**, not per transcript, because during a turn there
/// is always exactly one item changing: the assistant text growing by a delta,
/// or the running tool card's spinner and elapsed. A whole-transcript key
/// would be invalidated by that one item and re-wrap the entire session
/// anyway, which is the same cost it was meant to avoid. Per item, a frame
/// costs a hash of each item plus a re-wrap of only what moved.
///
/// Owned by the caller rather than by `State`: this is render state, and the
/// Elm model stays a pure description of the session. A fresh cache is always
/// correct — it just misses once.
#[derive(Default)]
pub struct TranscriptCache {
    geometry: Option<Geometry>,
    items: Vec<CachedItem>,
    rewraps: u64,
    line_wraps: u64,
}

impl TranscriptCache {
    /// How many individual items have been wrapped over this cache's life.
    /// Tests assert on it; nothing in the view reads it.
    pub fn rewraps(&self) -> u64 {
        self.rewraps
    }

    /// How many assistant lines have been classified+wrapped over this
    /// cache's life — the unit the incremental render economizes. Tests
    /// assert on it; nothing in the view reads it.
    pub fn line_wraps(&self) -> u64 {
        self.line_wraps
    }
}

struct CachedItem {
    fingerprint: u64,
    rows: Vec<Line<'static>>,
    /// Streaming state for a growing assistant item — `None` for every other
    /// kind. Dropped with the rows on any geometry change.
    incremental: Option<Incremental>,
}

/// Where an assistant item's frozen render ends. Rows below `frozen_rows`
/// belong to the trailing (possibly partial) line and are recomputed each
/// frame; everything above is exact because `Streamed` is append-only.
struct Incremental {
    /// Identity (construction seed) of the `Streamed` these rows came from —
    /// a different item at the same index must never inherit them.
    seed: u64,
    /// Byte offset of the first byte NOT yet frozen into `rows` — always at
    /// the start of the trailing (possibly partial) line.
    consumed: usize,
    /// Rows produced by the frozen prefix (can exceed its line count:
    /// wrapping).
    frozen_rows: usize,
    /// Fence state at `consumed` — the one piece of cross-line classifier
    /// state in `assistant_lines`.
    in_fence: bool,
}

impl Incremental {
    fn new(seed: u64) -> Self {
        Self {
            seed,
            consumed: 0,
            frozen_rows: 0,
            in_fence: false,
        }
    }
}

/// What every item's rows depend on beyond the item itself. A change here
/// drops the whole memo — all of it is stale at once.
///
/// Scroll position is deliberately absent: it picks the window out of the
/// cached rows, so scrolling never re-wraps anything.
#[derive(PartialEq)]
struct Geometry {
    width: u16,
    density: Density,
    thinking_expanded: bool,
    palette: Palette,
    /// The prose measure (0049 T5) — a `/reload` that changes it re-wraps.
    measure: usize,
    /// Whether an agent block advertises the band keys (0061 T8). Not a
    /// per-item property, so it belongs here rather than in the fingerprint.
    band_keys: bool,
}

/// A hash of everything about one item that reaches the screen.
///
/// Deriving this from content rather than from a hand-maintained revision
/// counter is the point: a new mutation site anywhere in `app` cannot forget
/// to invalidate the cache, because there is nothing to remember. Text fields
/// are `Streamed`, whose (seed, rev, len) is content-derived in O(1): the
/// field is private, so no mutation path can skip the revision bump, and the
/// construction-time seed keeps a replaced item from fingerprinting equal.
///
/// INVARIANT: every field `item_block` reads is hashed here, at the same
/// resolution it is rendered. Enforced by
/// `cached_rows_are_identical_to_a_fresh_render`, which walks a turn's worth
/// of mutations comparing a warm cache against a cold one.
fn item_fingerprint(item: &TranscriptItem) -> u64 {
    let text_key = |t: &crate::app::Streamed| (t.seed(), t.rev(), t.len() as u64);
    let mut h = DefaultHasher::new();
    match item {
        TranscriptItem::User { text } => (0u8, text_key(text)).hash(&mut h),
        TranscriptItem::Steer { text, queued } => (1u8, text_key(text), queued).hash(&mut h),
        TranscriptItem::Assistant { text } => (2u8, text_key(text)).hash(&mut h),
        TranscriptItem::Thinking { text } => (3u8, text_key(text)).hash(&mut h),
        TranscriptItem::Tool {
            id,
            name,
            summary,
            status,
            ticks,
            calls,
            children,
            // Drill-in only, like the tick stamps beside it: hashing a
            // child's live prose would re-wrap the parent card per byte.
            child_text: _,
            progress,
        } => {
            // Ticks are hashed at the two resolutions they are *rendered* at
            // — the marker's frame and the whole seconds of elapsed — not
            // raw. Hashing raw ticks would re-wrap the card on every frame of
            // a running tool, which is exactly when the cache has to hold.
            (
                4u8,
                id,
                name,
                summary,
                ticks / anim::TICK_HZ,
                marker_frame(*ticks),
            )
                .hash(&mut h);
            match status {
                ToolStatus::Running => 0u8.hash(&mut h),
                ToolStatus::Done => 1u8.hash(&mut h),
                ToolStatus::Failed => 2u8.hash(&mut h),
                ToolStatus::Denied => 3u8.hash(&mut h),
                // The depth is rendered, so it is hashed (0061 T24).
                ToolStatus::Queued { ahead } => (4u8, ahead).hash(&mut h),
                ToolStatus::AutoAllowed { rule } => (5u8, rule).hash(&mut h),
            }
            // 0039: everything `item_block` reads from the merged calls and
            // nested children. `started_at`/`settled_at` deliberately NOT
            // hashed — `item_block` never reads them; the drill-in renders
            // outside the cache (D7). `tokens` is excluded for the same
            // reason. Running-child glyphs ride the parent ticks hashed above.
            calls.len().hash(&mut h);
            for c in calls {
                (&c.id, c.ok, c.lines, c.bytes).hash(&mut h);
            }
            children.len().hash(&mut h);
            for c in children {
                (&c.id, &c.name, &c.summary, c.ok).hash(&mut h);
            }
            // 0061 T25: `bytes` is carried, never rendered, so it is not
            // hashed. `at_ticks` only moves on a frame that also moves
            // `tail`/`lines`, and the card's `quiet Ns` is a function of it
            // and the whole seconds already hashed above.
            match progress {
                None => 0u8.hash(&mut h),
                Some(pr) => (1u8, &pr.tail, pr.lines, pr.at_ticks).hash(&mut h),
            }
        }
        TranscriptItem::Notice { text } => (5u8, text_key(text)).hash(&mut h),
        // A new variant without its own discriminant byte would collide with
        // `Notice` and render a stale block after the numbers changed — a bug
        // no unit test catches and every use does.
        TranscriptItem::Report(r) => (6u8, r).hash(&mut h),
        TranscriptItem::Error { text } => (7u8, text_key(text)).hash(&mut h),
        TranscriptItem::WorkflowsReport(runs) => (8u8, runs).hash(&mut h),
        TranscriptItem::Plan(plan) => (9u8, plan).hash(&mut h),
        TranscriptItem::TurnSummary {
            secs,
            calls,
            finished_at,
        } => (10u8, secs, calls, finished_at).hash(&mut h),
    }
    h.finish()
}

pub fn view(state: &State, p: &Palette, cache: &mut TranscriptCache, frame: &mut Frame) {
    let area = frame.area();
    let [transcript, strip, _gap, input, selector, hint] = regions(state, area);
    // 0039: a selected spawn swaps the whole region above the strip for its
    // child stream (the Claude Code client pattern — full swap, no modal).
    // A dangling id falls back to the main transcript.
    match selected_spawn(state) {
        Some(item) => render_agent_stream(state, item, p, frame, transcript),
        None => render_transcript(state, p, cache, frame, transcript),
    }
    render_strip(state, p, frame, strip);
    render_input(state, p, frame, input);
    render_selector(state, p, frame, selector);
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
    // Over the whole frame (0049 T7): the table is wider and taller than
    // the transcript band alone can hold at 80×24.
    if state.help_open {
        render_help(state, p, frame, area);
    }
    if let Some(sel) = &state.selection {
        highlight(sel, frame);
    }
}

fn render_transcript(
    state: &State,
    p: &Palette,
    cache: &mut TranscriptCache,
    frame: &mut Frame,
    area: Rect,
) {
    // Wrapping up front (rather than via `Paragraph::wrap`) is what keeps the
    // scroll arithmetic honest: an item that overflows counts as the several
    // rows it really occupies, so Follow still lands on the last one. A
    // blank-line separator sits *between* items, and each item's start row is
    // derived from the rows above it for At-scroll.
    let geometry = Geometry {
        width: area.width,
        density: state.density,
        thinking_expanded: state.thinking_expanded,
        palette: *p,
        measure: state.measure,
        band_keys: state.selected_agent.is_none(),
    };
    let band_keys = geometry.band_keys;
    if cache.geometry.as_ref() != Some(&geometry) {
        cache.items.clear();
        cache.geometry = Some(geometry);
    }
    let width = area.width as usize;
    let gutter = state.density.gutter();
    // Blank rows above each item (0049 T2): one enum match per item per
    // frame — cheaper than the fingerprint hash the loop below already does.
    let blanks: Vec<usize> = (0..state.transcript.len())
        .map(|i| {
            blank_before(
                i.checked_sub(1).map(|j| &state.transcript[j]),
                &state.transcript[i],
                state.density,
            )
        })
        .collect();
    // A shorter transcript (`/clear`) drops the tail; the survivors keep their
    // rows, and any whose content changed is caught by its fingerprint below.
    cache.items.truncate(state.transcript.len());
    for (i, item) in state.transcript.iter().enumerate() {
        let fingerprint = item_fingerprint(item);
        if cache
            .items
            .get(i)
            .is_some_and(|c| c.fingerprint == fingerprint)
        {
            continue;
        }
        // Assistant prose renders incrementally: a growing item keeps its
        // frozen rows and classifies only what streamed in since the last
        // frame. Every other kind re-renders whole (small, or rare).
        let entry = if let TranscriptItem::Assistant { text } = item {
            let (mut rows, mut inc) = cache
                .items
                .get_mut(i)
                .and_then(|slot| {
                    // Only the same append-only item, merely grown, may keep
                    // its frozen prefix; anything else cold-renders.
                    slot.incremental
                        .take()
                        .filter(|inc| inc.seed == text.seed() && text.len() >= inc.consumed)
                        .map(|inc| (std::mem::take(&mut slot.rows), inc))
                })
                .unwrap_or_else(|| (Vec::new(), Incremental::new(text.seed())));
            cache.line_wraps +=
                assistant_append(&mut rows, &mut inc, text, p, width, gutter, state.measure);
            CachedItem {
                fingerprint,
                rows,
                incremental: Some(inc),
            }
        } else {
            CachedItem {
                fingerprint,
                rows: item_visual_lines(
                    item,
                    p,
                    width,
                    gutter,
                    state.thinking_expanded,
                    state.measure,
                    band_keys,
                ),
                incremental: None,
            }
        };
        match cache.items.get_mut(i) {
            Some(slot) => *slot = entry,
            None => cache.items.push(entry),
        }
        cache.rewraps += 1;
    }

    // Closed runs of settled cards collapse to one line each (0061 T6). This
    // is a row-selection pass over rows already cached, so folding and
    // unfolding costs nothing — the cached rows are identical either way,
    // which is why `Geometry` deliberately does not carry `tools_expanded`.
    let folds = fold_plan(&state.transcript, state.tools_expanded);
    let rollups: Vec<Option<Vec<Line>>> = folds
        .iter()
        .enumerate()
        .map(|(i, f)| match *f {
            RunFold::Head { end } => {
                Some(rollup_lines(&state.transcript[i..end], p, width, gutter))
            }
            _ => None,
        })
        .collect();
    let rows_for = |i: usize| -> &[Line] {
        match folds[i] {
            RunFold::Hidden => &[],
            RunFold::Head { .. } => rollups[i].as_deref().unwrap_or(&[]),
            RunFold::Show => &cache.items[i].rows,
        }
    };
    let blank_of = |i: usize| -> usize {
        if matches!(folds[i], RunFold::Hidden) {
            0
        } else {
            blanks[i]
        }
    };

    let height = area.height as usize;
    let total: usize = (0..cache.items.len())
        .map(|i| rows_for(i).len() + blank_of(i))
        .sum();
    // Each item above `idx` contributes its own rows plus the blank run
    // above it. A hidden item contributes neither, so `Scroll::At` on one
    // resolves to the row just after its rollup head.
    let start_of = |idx: usize| -> usize {
        (0..idx.min(cache.items.len()))
            .map(|i| rows_for(i).len() + blank_of(i))
            .sum()
    };
    let skip = match state.scroll {
        Scroll::Follow => total.saturating_sub(height),
        Scroll::At(item) if item < cache.items.len() => start_of(item),
        Scroll::At(_) => total,
    }
    .min(total.saturating_sub(1));
    // Walking to the window beats `Paragraph::scroll`, whose offset is a u16 a
    // long session would overflow. Only the rows actually on screen are
    // cloned, so the per-frame cost is bounded by the terminal, not by the
    // session. A transcript shorter than the viewport anchors to the strip
    // (0049 T4, LD2): the padding goes above it, so the newest row sits
    // just over the strip from the first prompt on and the first overflow
    // moves nothing that was already on screen.
    let pad = height.saturating_sub(total);
    let mut visible: Vec<Line> = (0..pad).map(|_| Line::raw("")).collect();
    let mut row = 0usize;
    'rows: for i in 0..cache.items.len() {
        for _ in 0..blank_of(i) {
            if visible.len() == height {
                break 'rows;
            }
            if row >= skip {
                visible.push(Line::raw(""));
            }
            row += 1;
        }
        let item_rows = rows_for(i);
        // Items entirely above the window are counted, never cloned.
        if row + item_rows.len() <= skip {
            row += item_rows.len();
            continue;
        }
        for line in item_rows {
            if visible.len() == height {
                break 'rows;
            }
            if row >= skip {
                visible.push(line.clone());
            }
            row += 1;
        }
    }
    frame.render_widget(Paragraph::new(visible), area);
}

/// A lone settled card is already one line; folding it would only cost the
/// reader its detail.
const MIN_FOLD: usize = 2;

/// What the fold pass decided for one transcript item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunFold {
    /// Render this item's own rows.
    Show,
    /// Render one rollup line standing in for `self..end`.
    Head { end: usize },
    /// Folded into the head above; no rows, no blank.
    Hidden,
}

/// Whether a card may disappear into a rollup: settled, successful, plain
/// work. Running, failed, denied and agent cards all say something a rollup
/// cannot, so they break a run.
fn foldable(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Tool { name, status, children, .. }
            if matches!(status, ToolStatus::Done)
                && !crate::app::is_agent_card(name)
                && children.is_empty()
    )
}

/// Maximal runs of foldable cards, collapsed to one line each. A run folds
/// only when it is *closed* — something follows it — so the work you are
/// watching right now never collapses under you; and only at `MIN_FOLD` or
/// more, because folding one card saves nothing.
///
/// Deliberately a view-time pass over the transcript rather than a reducer
/// item: a `ToolRun` item would force settle-by-id, `on_tick`, `band_spawns`
/// and `Scroll::At` to descend into runs, and closure depends on the *next*
/// item, which the reducer does not have when the card lands.
fn fold_plan(transcript: &[TranscriptItem], expanded: bool) -> Vec<RunFold> {
    let mut plan = vec![RunFold::Show; transcript.len()];
    if expanded {
        return plan;
    }
    let mut i = 0;
    while i < transcript.len() {
        if !foldable(&transcript[i]) {
            i += 1;
            continue;
        }
        let mut end = i;
        while end < transcript.len() && foldable(&transcript[end]) {
            end += 1;
        }
        let closed = end < transcript.len();
        if closed && end - i >= MIN_FOLD {
            plan[i] = RunFold::Head { end };
            for f in &mut plan[i + 1..end] {
                *f = RunFold::Hidden;
            }
        }
        i = end;
    }
    plan
}

/// `ran 4 shell commands, read 1 file · 12s` — one phrase per tool, in the
/// order the tools first appear, then the run's own elapsed. That elapsed is
/// tool time, not wall time: cards carry ticks, not absolute stamps.
fn rollup_text(run: &[TranscriptItem]) -> String {
    let mut order: Vec<&str> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    let mut ticks = 0u64;
    for item in run {
        let TranscriptItem::Tool {
            name,
            calls,
            ticks: t,
            ..
        } = item
        else {
            continue;
        };
        ticks += t;
        let verb = tool_verb(name);
        let n = match &verb.fold {
            Some(spec) if spec.per_card => 1,
            _ => calls.len(),
        };
        match order.iter().position(|o| *o == name.as_str()) {
            Some(at) => counts[at] += n,
            None => {
                order.push(name);
                counts.push(n);
            }
        }
    }
    let phrases: Vec<String> = order
        .iter()
        .zip(&counts)
        .filter_map(|(name, n)| tool_verb(name).phrase(*n))
        .collect();
    format!(
        "{} · {}",
        phrases.join(", "),
        fmt_elapsed(ticks / anim::TICK_HZ)
    )
}

/// `12s` · `2m 14s` · `1h 02m`. Shared with the turn summary so two readouts
/// of the same number never disagree.
fn fmt_elapsed(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// The rows one rollup occupies. Synthesized per frame and never cached: the
/// cards behind it keep their own cached rows, so a fold costs no re-wrap.
fn rollup_lines(
    run: &[TranscriptItem],
    p: &Palette,
    width: usize,
    gutter: usize,
) -> Vec<Line<'static>> {
    let spine = Spine {
        indent: 1,
        marker: SETTLED_OK_GLYPH,
        cont: " ",
        marker_style: Style::new().fg(p.muted),
        cont_style: Style::new(),
    };
    let inner = width.saturating_sub(gutter + spine.indent + 2).max(1);
    let content = Line::from(vec![
        Span::styled(rollup_text(run), Style::new().fg(p.muted)),
        Span::styled(" · ctrl-o", Style::new().fg(p.faint)),
    ]);
    let mut out = Vec::new();
    for wl in wrap::line(&content, inner) {
        let first = out.is_empty();
        out.push(spine.wrap(wl, gutter, first));
    }
    out
}

/// Who a transcript row belongs to. Blank rows fall only where this changes
/// (comfortable density) — a run of tool cards is one block, and an answer
/// follows the work it came from without a gap.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Speaker {
    You,
    Model,
    Harness,
}

fn speaker(item: &TranscriptItem) -> Speaker {
    match item {
        TranscriptItem::User { .. } | TranscriptItem::Steer { .. } => Speaker::You,
        TranscriptItem::Assistant { .. } => Speaker::Model,
        TranscriptItem::Thinking { .. }
        | TranscriptItem::Tool { .. }
        | TranscriptItem::Notice { .. }
        | TranscriptItem::Error { .. }
        | TranscriptItem::Report(_)
        | TranscriptItem::WorkflowsReport(_)
        | TranscriptItem::TurnSummary { .. }
        | TranscriptItem::Plan(_) => Speaker::Harness,
    }
}

/// Blank rows above `cur`. Compact: none. Spacious: one before every item.
/// Comfortable: one where the speaker changes, and always before a prompt.
fn blank_before(prev: Option<&TranscriptItem>, cur: &TranscriptItem, density: Density) -> usize {
    let Some(prev) = prev else { return 0 };
    match density {
        Density::Compact | Density::Spacious => density.blank_lines(),
        Density::Comfortable => {
            if speaker(prev) != speaker(cur) || matches!(cur, TranscriptItem::User { .. }) {
                density.blank_lines()
            } else {
                0
            }
        }
    }
}

/// The left-column signature of one turn: a marker glyph on the first visual
/// row, a continuation glyph on the rest, each with its own color. This is
/// what lets the eye track who is speaking by scanning straight down.
struct Spine {
    /// Columns of extra pad between the gutter and the glyph. Cards sit one
    /// column in from prose (0061 T3), so tool work reads as a second voice.
    indent: usize,
    marker: &'static str,
    cont: &'static str,
    marker_style: Style,
    cont_style: Style,
}

impl Spine {
    /// Prepend the gutter pad and this row's spine glyph to a content line.
    /// The glyph occupies one column; a trailing space separates it from the
    /// text, so content always starts at `gutter + indent + 2`.
    fn wrap<'a>(&self, mut content: Line<'a>, gutter: usize, first: bool) -> Line<'a> {
        let (glyph, style) = if first {
            (self.marker, self.marker_style)
        } else {
            (self.cont, self.cont_style)
        };
        let lead = format!("{}{glyph} ", " ".repeat(gutter + self.indent));
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
/// gutter+spine leave — prose no wider than the measure (0049 T5, LD3),
/// cards, code and reports at the full width — each row carrying its spine
/// glyph. Used by both the render and the scroll math, so they can never
/// disagree on row counts.
fn item_visual_lines<'a>(
    item: &TranscriptItem,
    p: &Palette,
    width: usize,
    gutter: usize,
    thinking_expanded: bool,
    measure: usize,
    band_keys: bool,
) -> Vec<Line<'a>> {
    // `gutter + 2` = the pad plus the one-column glyph and its trailing space;
    // a card's own indent narrows it further, so `inner` is known before the
    // block is built and `item_block` can clip to the width it will get.
    let inner = width.saturating_sub(gutter + item_indent(item) + 2).max(1);
    let (spine, content) = item_block(item, p, thinking_expanded, inner, band_keys);
    let mut out = Vec::new();
    for (cl, full) in &content {
        for wl in wrap::line(cl, if *full { inner } else { inner.min(measure) }) {
            let first = out.is_empty();
            out.push(spine.wrap(wl, gutter, first));
        }
    }
    if out.is_empty() {
        out.push(spine.wrap(Line::raw(""), gutter, true));
    }
    out
}

/// Content rows tagged with whether they keep the full width (`true`: cards,
/// code, reports) or wrap at the prose measure (`false`).
type Tagged<'a> = Vec<(Line<'a>, bool)>;

fn prose(lines: Vec<Line<'_>>) -> Tagged<'_> {
    lines.into_iter().map(|l| (l, false)).collect()
}

fn full(lines: Vec<Line<'_>>) -> Tagged<'_> {
    lines.into_iter().map(|l| (l, true)).collect()
}

/// Assistant prose with light, line-level structure so an answer is scannable
/// on its own, not just at the turn boundary. Deliberately NOT a markdown
/// engine: each line is classified by how it begins, nothing spans lines
/// except the fenced-code toggle. Anything unrecognized stays plain ink, so a
/// stray `#` mid-sentence never turns into a heading.
fn assistant_lines<'a>(text: &str, p: &Palette) -> Tagged<'a> {
    let mut in_fence = false;
    text.split('\n')
        .map(|raw| assistant_line(raw, &mut in_fence, p))
        .collect()
}

/// One classified assistant line and whether it is full-width (a fence line,
/// fenced code, or 4-space-indented code); `in_fence` is the only state
/// carried across lines, which is what lets the incremental render re-enter
/// mid-text.
fn assistant_line<'a>(raw: &str, in_fence: &mut bool, p: &Palette) -> (Line<'a>, bool) {
    let lead = raw.trim_start();
    // ``` toggles a code fence; the fence line itself renders as a quiet
    // divider rather than literal backticks shouting on screen.
    if lead.starts_with("```") {
        *in_fence = !*in_fence;
        return (
            Line::styled(raw.to_string(), Style::new().fg(p.faint).dim()),
            true,
        );
    }
    if *in_fence {
        return (code_line(raw, p), true);
    }
    // `#`..`###`-led heading → bold, hashes stripped.
    if let Some(h) = heading_text(lead) {
        return (Line::styled(h, Style::new().fg(p.ink).bold()), false);
    }
    // `- ` / `* ` bullet → a `•` marker in the accent, indentation kept.
    if let Some((indent, rest)) = bullet(raw) {
        let mut spans = vec![
            Span::raw(indent.to_string()),
            Span::styled("• ", Style::new().fg(p.accent)),
        ];
        spans.extend(inline_spans(rest, Style::new().fg(p.ink), p));
        return (Line::from(spans), false);
    }
    // `1. ` numbered item → the number in the accent, the same as a bullet's
    // marker. Deliberately after the bullet check and before the code form.
    if let Some((indent, marker, rest)) = numbered(raw) {
        let mut spans = vec![
            Span::raw(indent.to_string()),
            Span::styled(format!("{marker} "), Style::new().fg(p.accent)),
        ];
        spans.extend(inline_spans(rest, Style::new().fg(p.ink), p));
        return (Line::from(spans), false);
    }
    // A 4-space indent is markdown's other code form.
    if raw.starts_with("    ") && !raw.trim().is_empty() {
        return (code_line(raw, p), true);
    }
    (
        Line::from(inline_spans(raw, Style::new().fg(p.ink), p)),
        false,
    )
}

/// `(leading_indent, marker, item_text)` for a `1. ` / `12) ` numbered item.
/// Requires digits then `.`/`)` then a space, so `3.14` in prose is not a
/// list — the same conservatism as `heading_text`.
fn numbered(raw: &str) -> Option<(&str, &str, &str)> {
    let indent = &raw[..raw.len() - raw.trim_start().len()];
    let lead = &raw[indent.len()..];
    let digits = lead.len() - lead.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let rest = &lead[digits..];
    let punct = rest.chars().next()?;
    if punct != '.' && punct != ')' {
        return None;
    }
    let body = rest[punct.len_utf8()..].strip_prefix(' ')?;
    Some((indent, &lead[..digits + punct.len_utf8()], body))
}

/// One prose line's spans: `` `code` `` in the accent (backticks stripped) and
/// `**bold**` bold in the `strong` role. An unbalanced marker is literal text
/// — half a span pair is a typo, not markup, and eating it would lose
/// characters the model wrote.
fn inline_spans<'a>(raw: &str, base: Style, p: &Palette) -> Vec<Span<'a>> {
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut rest = raw;
    while !rest.is_empty() {
        let marker = rest
            .find("**")
            .map(|at| (at, "**"))
            .into_iter()
            .chain(rest.find('`').map(|at| (at, "`")))
            .min_by_key(|(at, _)| *at);
        let Some((at, mark)) = marker else { break };
        let after = &rest[at + mark.len()..];
        let Some(end) = after.find(mark).filter(|e| *e > 0) else {
            // Unbalanced (or empty `` `` ``): the marker is literal.
            plain.push_str(&rest[..at + mark.len()]);
            rest = after;
            continue;
        };
        plain.push_str(&rest[..at]);
        if !plain.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut plain), base));
        }
        let inner = after[..end].to_string();
        spans.push(match mark {
            "`" => Span::styled(inner, Style::new().fg(p.accent)),
            _ => Span::styled(inner, Style::new().fg(p.strong).bold()),
        });
        rest = &after[end + mark.len()..];
    }
    plain.push_str(rest);
    if !plain.is_empty() || spans.is_empty() {
        spans.push(Span::styled(plain, base));
    }
    spans
}

/// Classify+wrap only what grew since the last frame: newly *completed* lines
/// are frozen once (rows appended, fence state and byte cursor advanced); the
/// trailing partial line is re-rendered every frame and replaced on the next.
/// Returns the number of lines classified, the unit `line_wraps` counts.
///
/// Equivalence with a cold `item_visual_lines` render is the tested invariant
/// (`incremental_assistant_rows_equal_cold_render`): same split, same
/// classifier, same wrap, same spine-first rule.
fn assistant_append(
    rows: &mut Vec<Line<'static>>,
    inc: &mut Incremental,
    text: &str,
    p: &Palette,
    width: usize,
    gutter: usize,
    measure: usize,
) -> u64 {
    let inner = width.saturating_sub(gutter + 2).max(1);
    let width_of = |full: bool| if full { inner } else { inner.min(measure) };
    let spine = assistant_spine(p);
    let mut classified = 0u64;
    // Drop the previous frame's partial-line rows; the frozen prefix stands.
    rows.truncate(inc.frozen_rows);
    let tail = &text[inc.consumed..];
    if let Some(nl) = tail.rfind('\n') {
        for raw in tail[..nl].split('\n') {
            let (cl, full) = assistant_line(raw, &mut inc.in_fence, p);
            classified += 1;
            for wl in wrap::line(&cl, width_of(full)) {
                let first = rows.is_empty();
                rows.push(spine.wrap(wl, gutter, first));
            }
        }
        inc.consumed += nl + 1;
        inc.frozen_rows = rows.len();
    }
    // The trailing (possibly partial) line. Its fence toggle must not leak
    // into frozen state — the line may still grow into something else.
    let mut fence = inc.in_fence;
    let (cl, full) = assistant_line(&text[inc.consumed..], &mut fence, p);
    classified += 1;
    for wl in wrap::line(&cl, width_of(full)) {
        let first = rows.is_empty();
        rows.push(spine.wrap(wl, gutter, first));
    }
    classified
}

/// The assistant spine — shared by `item_block` and the incremental path so
/// they cannot drift.
fn assistant_spine(p: &Palette) -> Spine {
    // The warm dot + a faint bar down the whole answer, so a long reply
    // reads as one block rather than a wall of flat text.
    Spine {
        indent: 0,
        marker: "●",
        cont: "│",
        marker_style: Style::new().fg(p.accent),
        cont_style: Style::new().fg(p.faint),
    }
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

/// Columns a card sits in from the prose column. One source of truth with
/// `Spine::indent` — `every_items_indent_matches_its_spine` holds them equal.
fn item_indent(item: &TranscriptItem) -> usize {
    usize::from(matches!(item, TranscriptItem::Tool { .. }))
}

/// A count with thousands separators — `1204` reads as `1,204` at a glance.
fn group_digits(n: u64) -> String {
    let raw = n.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The spine and the content spans for one item — the content no longer
/// carries its own marker prefix; the spine owns that column now.
fn item_block<'a>(
    item: &TranscriptItem,
    p: &Palette,
    thinking_expanded: bool,
    inner: usize,
    band_keys: bool,
) -> (Spine, Tagged<'a>) {
    match item {
        TranscriptItem::User { text } => (
            // You are the anchor: high-contrast caret, no continuation bar.
            Spine {
                indent: 0,
                marker: "❯",
                cont: " ",
                marker_style: Style::new().fg(p.ink).bold(),
                cont_style: Style::new(),
            },
            prose(
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
        ),
        TranscriptItem::Assistant { text } => (assistant_spine(p), assistant_lines(text, p)),
        TranscriptItem::Steer { text, queued: true } => (
            Spine {
                indent: 0,
                marker: "⤷",
                cont: " ",
                marker_style: Style::new().fg(p.muted),
                cont_style: Style::new(),
            },
            prose(vec![token_line(
                format!("{text} · queued"),
                Style::new().fg(p.muted),
                Style::new().fg(p.accent),
            )]),
        ),
        TranscriptItem::Steer {
            text,
            queued: false,
        } => (
            Spine {
                indent: 0,
                marker: "⤷",
                cont: " ",
                marker_style: Style::new().fg(p.accent),
                cont_style: Style::new(),
            },
            prose(vec![token_line(
                text.to_string(),
                Style::new().fg(p.accent),
                Style::new().fg(p.accent).bold(),
            )]),
        ),
        TranscriptItem::Tool {
            id: _,
            name,
            summary,
            status,
            ticks,
            calls,
            children,
            child_text: _,
            progress,
        } => {
            let running = matches!(status, ToolStatus::Running | ToolStatus::AutoAllowed { .. });
            let (marker, color) = status_glyph(status, *ticks, p);
            let (body, mut details) = split_summary(name, summary);
            // The `cd` prefix is identical on every call in a session and eats
            // the summary; the directory it lands in is the informative part.
            let body = if name == "bash" {
                bash_body(&body)
            } else {
                body
            };
            if let ToolStatus::AutoAllowed { rule } = status {
                details.push(format!("auto-allowed: {rule}"));
            }
            if let ToolStatus::Queued { ahead } = status {
                details.push(match ahead {
                    0 => "queued · next".to_string(),
                    n => format!("queued behind {n}"),
                });
            }
            // The merged multiplier (0039 D3): one card, N absorbed calls.
            if calls.len() > 1 {
                details.push(format!("×{}", calls.len()));
            }
            // An agent card is a two-row block (0061 T8): the brief keeps the
            // title row, and everything about the delegation drops to a
            // second row under a bar. Every other card stays one line plus
            // its result row.
            let agent = crate::app::is_agent_card(name);
            if !agent {
                // Elapsed rides the header only while the tool is still
                // running; a settled card moves it to the result row (T3).
                if running {
                    details.push(format!("{}s", ticks / anim::TICK_HZ));
                    if let Some(pr) = progress {
                        details.push(format!("{} lines", group_digits(pr.lines)));
                    }
                }
            }
            // Name in the status color (so it stays identifiable now the
            // marker moved to the spine); body and details both muted, so the
            // whole card reads as a second voice under the prose (0061 T3).
            let mut spans = vec![Span::styled(tool_verb(name).label, Style::new().fg(color))];
            if !body.is_empty() {
                let style = if agent {
                    Style::new().fg(p.ink)
                } else {
                    Style::new().fg(p.muted)
                };
                spans.push(Span::styled(format!("  {body}"), style));
            }
            if !details.is_empty() {
                spans.push(Span::styled(
                    format!(" · {}", details.join(" · ")),
                    Style::new().fg(p.muted),
                ));
            }
            // The one loud detail on a muted card (0061 T25): only `bash`
            // has a sink, so only its silence means anything. Appended after
            // the muted details so it can carry its own style.
            if running && !agent && name == "bash" {
                let since = match progress {
                    Some(pr) => ticks.saturating_sub(pr.at_ticks),
                    None => *ticks,
                };
                if since >= anim::QUIET_AFTER {
                    spans.push(Span::styled(
                        format!(" · quiet {}s", since / anim::TICK_HZ),
                        Style::new().fg(p.blocked),
                    ));
                }
            }
            let mut rows = vec![Line::from(spans)];
            // The tail row (0061 T25) takes the same slot the result row will
            // take once the tool settles, so the block's height never jumps.
            // Clipped, never wrapped: a 200-char tail would otherwise flip
            // between one and three rows per frame.
            if running && !agent {
                if let Some(pr) = progress.as_ref().filter(|pr| !pr.tail.is_empty()) {
                    let clipped: String = pr.tail.chars().take(inner).collect();
                    rows.push(Line::styled(clipped, Style::new().fg(p.faint)));
                }
            }
            if agent {
                rows.push(Line::styled(
                    agent_row(children, *ticks, running, band_keys),
                    Style::new().fg(p.muted),
                ));
            } else if let Some(text) = result_row(status, calls, *ticks) {
                // The result row (0061 T3): what the model got, and how long
                // it took. Settled cards only — a running one owns its clock.
                rows.push(Line::styled(text, Style::new().fg(p.faint)));
            }
            (
                Spine {
                    indent: 1,
                    marker,
                    cont: if agent { "│" } else { " " },
                    marker_style: Style::new().fg(color),
                    cont_style: if agent {
                        Style::new().fg(p.faint)
                    } else {
                        Style::new()
                    },
                },
                full(rows),
            )
        }
        TranscriptItem::Notice { text } => (
            Spine {
                indent: 0,
                marker: "·",
                cont: " ",
                marker_style: Style::new().fg(p.muted),
                cont_style: Style::new(),
            },
            prose(vec![Line::styled(
                text.to_string(),
                Style::new().fg(p.muted).italic(),
            )]),
        ),
        // A failed turn: red with a ✗, never the muted notice spine, so an
        // execution error cannot be mistaken for the routine chatter near it.
        TranscriptItem::Error { text } => (
            Spine {
                indent: 0,
                marker: "✗",
                cont: " ",
                marker_style: Style::new().fg(p.blocked).bold(),
                cont_style: Style::new(),
            },
            prose(vec![Line::styled(
                text.to_string(),
                Style::new().fg(p.blocked).bold(),
            )]),
        ),
        // A `/context` report. Harness output, so it takes the `Notice` spine
        // rather than anything that reads as the model speaking.
        TranscriptItem::Report(r) => (
            Spine {
                indent: 0,
                marker: "·",
                cont: "·",
                marker_style: Style::new().fg(p.muted),
                cont_style: Style::new().fg(p.muted),
            },
            full(report_lines(r, p, inner)),
        ),
        // A `/workflows` report (0044): one row per run, the same spine.
        TranscriptItem::WorkflowsReport(runs) => (
            Spine {
                indent: 0,
                marker: "·",
                cont: "·",
                marker_style: Style::new().fg(p.muted),
                cont_style: Style::new().fg(p.muted),
            },
            full(workflow_lines(runs, p)),
        ),
        // The plan card (0056 T3). A live one carries its two keys; an
        // answered one keeps the plan and drops them.
        TranscriptItem::Plan(plan) => (
            Spine {
                indent: 0,
                marker: "◆",
                cont: "│",
                marker_style: Style::new().fg(p.accent).bold(),
                cont_style: Style::new().fg(p.accent),
            },
            full(plan_lines(plan, p)),
        ),
        // The turn's full stop (0061 T9): one faint line at the prose column
        // saying what the turn cost. Harness voice, never the model's.
        TranscriptItem::TurnSummary {
            secs,
            calls,
            finished_at,
        } => {
            let mut parts = vec![fmt_elapsed(*secs)];
            if *calls > 0 {
                parts.push(call_count(*calls));
            }
            if let Some(at) = finished_at {
                parts.push(format!("done {at}"));
            }
            (
                Spine {
                    indent: 0,
                    marker: "✻",
                    cont: " ",
                    marker_style: Style::new().fg(p.faint),
                    cont_style: Style::new(),
                },
                prose(vec![Line::styled(
                    parts.join(" · "),
                    Style::new().fg(p.faint),
                )]),
            )
        }
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
                    indent: 0,
                    marker: "·",
                    cont: " ",
                    marker_style: Style::new().fg(p.faint),
                    cont_style: Style::new(),
                },
                prose(lines),
            )
        }
    }
}

/// The `└ N lines · Ns` row under a settled card: what the model received
/// and how long the call took. `None` for a running card (its clock is still
/// in the header), for a denial (nothing ran), and for an older peer that
/// sent no counts — absent counts are not a claim of zero.
fn result_row(status: &ToolStatus, calls: &[crate::app::ToolCall], ticks: u64) -> Option<String> {
    if !matches!(status, ToolStatus::Done | ToolStatus::Failed) {
        return None;
    }
    if !calls.iter().any(|c| c.lines.is_some()) {
        return None;
    }
    let lines: u64 = calls.iter().filter_map(|c| c.lines).sum();
    let bytes: u64 = calls.iter().filter_map(|c| c.bytes).sum();
    let what = if bytes == 0 {
        "no output".to_string()
    } else if lines == 1 {
        "1 line".to_string()
    } else {
        format!("{} lines", group_digits(lines))
    };
    Some(format!("└ {what} · {}s", ticks / anim::TICK_HZ))
}

/// The second row of an agent block: how much work it has done, how long it
/// has been at it, what it is doing now, and — while it runs and nothing is
/// drilled into — the keys that open it.
fn agent_row(
    children: &[crate::app::ChildCall],
    ticks: u64,
    running: bool,
    band_keys: bool,
) -> String {
    let mut parts = Vec::new();
    if !children.is_empty() {
        parts.push(call_count(children.len()));
    }
    parts.push(fmt_elapsed(ticks / anim::TICK_HZ));
    if running {
        // The newest outstanding child, else the last one to have run.
        if let Some(c) = children
            .iter()
            .rev()
            .find(|c| c.ok.is_none())
            .or_else(|| children.last())
        {
            let (body, _) = split_summary(&c.name, &c.summary);
            let what = if body.is_empty() {
                c.summary.clone()
            } else {
                body
            };
            parts.push(format!("{} {what}", tool_verb(&c.name).label));
        }
        if band_keys {
            parts.push("↑↓ agents".into());
        }
    }
    parts.join(" · ")
}

/// `1 call` / `N calls` — a spawn's child count on its card and band row.
fn call_count(n: usize) -> String {
    if n == 1 {
        "1 call".into()
    } else {
        format!("{n} calls")
    }
}

/// `name · status · Review 4/4 ✓ → Verify 6/7 ✗ · 41.2k tok · 3m12s` per run.
/// A phase is `✓` once every started agent settled without failure, `✗`
/// when any failed, and bare while it is still running.
fn plan_lines<'a>(plan: &crate::app::PresentedPlan, p: &Palette) -> Vec<Line<'a>> {
    let mut out = vec![Line::styled(
        format!("Plan — {} steps", plan.steps.len()),
        Style::new().fg(p.ink).bold(),
    )];
    for l in plan.summary.split('\n') {
        out.push(Line::styled(l.to_string(), Style::new().fg(p.ink)));
    }
    out.push(Line::raw(""));
    for (i, step) in plan.steps.iter().enumerate() {
        out.push(Line::styled(
            format!("{}. {}", i + 1, step.content),
            Style::new().fg(p.ink),
        ));
        if let Some(proof) = &step.proof {
            out.push(Line::styled(
                format!("   → {proof}"),
                Style::new().fg(p.muted),
            ));
        }
        if !step.after.is_empty() {
            out.push(Line::styled(
                format!("   ⇐ after {}", step.after.join(", ")),
                Style::new().fg(p.muted),
            ));
        }
    }
    if let Some(path) = &plan.path {
        out.push(Line::styled(
            format!("saved to {path}"),
            Style::new().fg(p.faint),
        ));
    }
    if plan.live {
        out.push(Line::styled(
            "a approve · r revise (or just say what to change)",
            Style::new().fg(p.accent).bold(),
        ));
    }
    out
}

fn workflow_lines<'a>(runs: &[crate::app::WorkflowRun], p: &Palette) -> Vec<Line<'a>> {
    let mut out = vec![Line::styled(
        format!(
            "Workflows — {} run{}",
            runs.len(),
            if runs.len() == 1 { "" } else { "s" }
        ),
        Style::new().fg(p.ink).bold(),
    )];
    for r in runs {
        let status_color = match r.status.as_str() {
            "done" => p.idle,
            "failed" | "cancelled" => p.blocked,
            _ => p.active,
        };
        let phases: Vec<String> = r
            .phases
            .iter()
            .map(|ph| {
                let mark = if ph.failed > 0 {
                    " ✗"
                } else if ph.started > 0 && ph.settled == ph.started {
                    " ✓"
                } else {
                    ""
                };
                format!("{} {}/{}{mark}", ph.title, ph.settled, ph.started)
            })
            .collect();
        let secs = r.elapsed_ms / 1000;
        let elapsed = if secs >= 60 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        };
        out.push(Line::from(vec![
            Span::styled(r.name.clone(), Style::new().fg(p.ink).bold()),
            Span::styled(format!(" · {}", r.status), Style::new().fg(status_color)),
            Span::styled(
                format!(
                    " · {} · {} tok · {elapsed}",
                    phases.join(" → "),
                    crate::app::tok(r.tokens)
                ),
                Style::new().fg(p.muted),
            ),
        ]));
    }
    out
}

/// Display labels for `/context` rows. The wire tag (`ContextKind`'s
/// `snake_case` serde name) is never shown — it is a protocol contract, and
/// this is a table of prose.
fn label(kind: ContextKind) -> &'static str {
    match kind {
        ContextKind::SystemPrompt => "system prompt",
        ContextKind::ToolSchemas => "tool schemas",
        ContextKind::SkillsRoster => "skills roster",
        ContextKind::AgentsRoster => "agents roster",
        ContextKind::ProjectInstructions => "project instructions",
        ContextKind::Memory => "memory",
        ContextKind::Todos => "todos",
        ContextKind::FoldedHistory => "folded history",
        ContextKind::Messages => "messages",
        ContextKind::ToolResults => "tool results",
        ContextKind::HarnessInjections => "harness injections",
        ContextKind::Images => "images",
        // A row from a newer engine. Named for what it is to this binary.
        ContextKind::Unknown => "other",
    }
}

/// Share of the window, as a percentage. A zero window is a misconfigured
/// engine, not a crash: every share is then 0.
fn share(n: u64, window: u64) -> f64 {
    match window {
        0 => 0.0,
        w => n as f64 * 100.0 / w as f64,
    }
}

/// The free-space row is the only one not on the wire — it is the difference
/// between the window and whichever total is larger.
const FREE_LABEL: &str = "free space";

/// Below this share of the window, free space stops being an identity and
/// becomes a warning. The one place color in this block carries urgency.
const FREE_ALARM_PCT: f64 = 15.0;

/// A meter narrower than this lies more than it tells — `view.rs` already
/// treats "too narrow, drop the chip" as the house rule for exactly this.
const MIN_METER_COLS: usize = 24;

/// Which band of the context a row belongs to. Shape encodes the group so the
/// grouping survives a monochrome terminal or a colorblind reader; color then
/// separates rows *within* a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// Rebuilt identically every turn, cached: the byte-stable prefix.
    Prefix,
    /// Assembled once per session from disk and state.
    Preamble,
    /// Everything the session itself produced.
    Conversation,
}

impl Group {
    const ALL: [Group; 3] = [Group::Prefix, Group::Preamble, Group::Conversation];

    fn glyph(self) -> &'static str {
        match self {
            Group::Prefix => "▣",
            Group::Preamble => "◆",
            Group::Conversation => "▪",
        }
    }

    /// The hue every row in the group slides away from.
    fn anchor(self, p: &Palette) -> Color {
        match self {
            Group::Prefix => p.accent,
            Group::Preamble => p.idle,
            Group::Conversation => p.active,
        }
    }
}

const FREE_GLYPH: &str = "▫";

fn group(kind: ContextKind) -> Group {
    match kind {
        ContextKind::SystemPrompt
        | ContextKind::ToolSchemas
        | ContextKind::SkillsRoster
        | ContextKind::AgentsRoster => Group::Prefix,
        ContextKind::ProjectInstructions | ContextKind::Memory | ContextKind::Todos => {
            Group::Preamble
        }
        ContextKind::FoldedHistory
        | ContextKind::Messages
        | ContextKind::ToolResults
        | ContextKind::HarnessInjections
        | ContextKind::Images
        | ContextKind::Unknown => Group::Conversation,
    }
}

struct ReportRow {
    label: &'static str,
    tokens: u64,
    glyph: &'static str,
    color: Color,
}

/// The display rows, free space last. Eight `Palette` slots cannot give twelve
/// distinguishable colors, so each group's rows slide from its anchor toward
/// the ink: `i / n * 0.45`. The 0.45 cap is what stops the last row of a long
/// group reading as plain text.
fn report_rows(r: &ContextReport, p: &Palette) -> Vec<ReportRow> {
    let groups: Vec<Group> = r.rows.iter().map(|(k, _)| group(*k)).collect();
    let mut seen = [0usize; Group::ALL.len()];
    let mut out: Vec<ReportRow> = r
        .rows
        .iter()
        .zip(&groups)
        .map(|((kind, tokens), g)| {
            let slot = Group::ALL
                .iter()
                .position(|x| x == g)
                .expect("a real group");
            let i = seen[slot];
            seen[slot] += 1;
            // `max(2)` so a lone row in a group keeps its anchor undiluted
            // rather than jumping the full 0.45 toward the ink.
            let n = groups.iter().filter(|x| *x == g).count().max(2);
            ReportRow {
                label: label(*kind),
                tokens: *tokens,
                glyph: g.glyph(),
                color: hotl_theme::blend(g.anchor(p), p.ink, i as f64 / n as f64 * 0.45),
            }
        })
        .collect();
    out.push(ReportRow {
        label: FREE_LABEL,
        tokens: r.free,
        glyph: FREE_GLYPH,
        color: if share(r.free, r.window) < FREE_ALARM_PCT {
            p.blocked
        } else {
            p.faint
        },
    });
    out
}

/// Largest-remainder apportionment of `cells` across `weights`. Rounding down
/// and handing the leftovers to the biggest fractions is what stops a 0.4% row
/// eating a whole cell from a 40% one — and lets a row that rounds to nothing
/// be genuinely absent rather than rounded up to a lie.
fn allocate(weights: &[u64], total: u64, cells: usize) -> Vec<usize> {
    if total == 0 || cells == 0 {
        return vec![0; weights.len()];
    }
    let mut base = Vec::with_capacity(weights.len());
    let mut fracs: Vec<(f64, usize)> = Vec::with_capacity(weights.len());
    let mut floor_sum = 0usize;
    for (i, w) in weights.iter().enumerate() {
        let exact = *w as f64 * cells as f64 / total as f64;
        let whole = exact.floor() as usize;
        base.push(whole);
        floor_sum += whole;
        fracs.push((exact - whole as f64, i));
    }
    fracs.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, i) in fracs.into_iter().take(cells.saturating_sub(floor_sum)) {
        base[i] += 1;
    }
    base
}

/// The screenshot's block grid compressed into one row: `▇` per used segment
/// in row order and row color, `▁` for what is left. The only part of the
/// block carrying information the table does not.
fn meter<'a>(r: &ContextReport, rows: &[ReportRow], p: &Palette, cols: usize) -> Line<'a> {
    // Rows and free space account for `estimated + free`. When the provider
    // reported MORE than the estimator did, the difference is real occupancy
    // no row can name — show it rather than silently rescaling everything.
    let accounted = r.estimated + r.free;
    let mut weights: Vec<u64> = Vec::with_capacity(rows.len() + 1);
    let mut styles: Vec<(&'static str, Color)> = Vec::with_capacity(rows.len() + 1);
    for row in &rows[..rows.len() - 1] {
        weights.push(row.tokens);
        styles.push(("▇", row.color));
    }
    if r.window > accounted {
        weights.push(r.window - accounted);
        styles.push(("▇", p.muted));
    }
    let free = rows.last().expect("free space is always a row");
    weights.push(free.tokens);
    styles.push(("▁", free.color));

    let cells = allocate(&weights, weights.iter().sum(), cols);
    let mut spans = vec![Span::raw("  ")];
    for (n, (glyph, color)) in cells.into_iter().zip(styles) {
        if n > 0 {
            spans.push(Span::styled(glyph.repeat(n), Style::new().fg(color)));
        }
    }
    Line::from(spans)
}

/// The `/context` block: a header, a meter, the two totals, and one line per
/// non-zero row plus free space. Column widths are computed from what is
/// actually shown, because row visibility varies session to session.
fn report_lines<'a>(r: &ContextReport, p: &Palette, inner: usize) -> Vec<Line<'a>> {
    let rows = report_rows(r, p);
    let lw = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    let nw = rows
        .iter()
        .map(|row| crate::app::tok(row.tokens).len())
        .max()
        .unwrap_or(0);

    let mut out = vec![
        Line::styled(
            format!(
                "Context Usage — {} · {} window",
                r.model,
                crate::app::tok(r.window)
            ),
            Style::new().fg(p.ink).bold(),
        ),
        Line::raw(""),
    ];
    // A two-cell bar would be a worse answer than no bar.
    if inner >= MIN_METER_COLS {
        out.push(meter(r, &rows, p, inner - 2));
        out.push(Line::raw(""));
    }
    // Absent before the first turn: the provider has reported nothing yet, and
    // an invented zero would read as an empty context.
    if let Some(reported) = r.reported {
        out.push(total_line("reported", reported, r.window, "last turn", p));
    }
    out.push(total_line(
        "estimated",
        r.estimated,
        r.window,
        "rows below",
        p,
    ));
    out.push(Line::raw(""));
    for row in rows {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(row.glyph, Style::new().fg(row.color)),
            Span::raw(" "),
            Span::styled(
                format!("{:<lw$}", row.label, lw = lw),
                Style::new().fg(row.color),
            ),
            Span::styled(
                // The percentage is right-aligned as a whole, so the paren
                // moves and the digits line up — padding *inside* the parens
                // would align the digits but read as a typo.
                format!(
                    "  {n:>nw$}  {s:>7}",
                    n = crate::app::tok(row.tokens),
                    nw = nw,
                    s = format!("({:.1}%)", share(row.tokens, r.window)),
                ),
                Style::new().fg(p.muted),
            ),
        ]));
    }
    out
}

/// One of the two totals. Whole-number percentages here, one decimal on the
/// rows: the totals are the headline, the rows are the accounting.
fn total_line<'a>(name: &str, n: u64, window: u64, note: &str, p: &Palette) -> Line<'a> {
    Line::styled(
        format!(
            "  {name:<9}  {n:>8} / {w}  ({s:.0}%)  {note}",
            name = name,
            n = crate::app::tok(n),
            w = crate::app::tok(window),
            s = share(n, window),
            note = note,
        ),
        Style::new().fg(p.ink),
    )
}

/// A card's verb and the phrase a rollup uses for a run of them — one table,
/// so the collapsed line and the expanded card can never disagree about what
/// a tool is called. `split_summary`, `merge_key`, settle-by-id, the ask modal
/// and the drill-in header all keep the raw lowercase name: this is a
/// rendering choice, never an identity one.
struct ToolVerb {
    /// Title-case, past tense where the tool leaves something behind.
    label: String,
    /// `None` = never folds into a rollup (agent and workflow cards).
    fold: Option<FoldSpec>,
}

/// How a run of one tool's cards reads once folded. An empty `noun` takes the
/// fallback form — `3 Skill calls` — for tools with no natural verb.
struct FoldSpec {
    verb: &'static str,
    noun: &'static str,
    /// Count cards, not absorbed calls: a merged read is one file read
    /// several ways, and saying "read 5 files" of it would be a lie.
    per_card: bool,
}

impl ToolVerb {
    /// The rollup phrase for `n` of these.
    fn phrase(&self, n: usize) -> Option<String> {
        let spec = self.fold.as_ref()?;
        let s = if n == 1 { "" } else { "s" };
        Some(if spec.noun.is_empty() {
            format!("{n} {} call{s}", self.label)
        } else {
            format!("{} {n} {}{s}", spec.verb, spec.noun)
        })
    }
}

fn tool_verb(name: &str) -> ToolVerb {
    let known = |label: &str, verb, noun, per_card| ToolVerb {
        label: label.into(),
        fold: Some(FoldSpec {
            verb,
            noun,
            per_card,
        }),
    };
    match name {
        "bash" => known("Bash", "ran", "shell command", false),
        "read" => known("Read", "read", "file", true),
        "write" => known("Wrote", "wrote", "file", true),
        "edit" => known("Edited", "edited", "file", true),
        "grep" => known("Searched", "searched", "pattern", false),
        "glob" => known("Listed", "listed", "pattern", false),
        "spawn" => ToolVerb {
            label: "Agent".into(),
            fold: None,
        },
        "workflow" => ToolVerb {
            label: "Workflow".into(),
            fold: None,
        },
        other => known(
            &other
                .split('_')
                .map(title_case)
                .collect::<Vec<_>>()
                .join(" "),
            "",
            "",
            false,
        ),
    }
}

/// One `_`-separated word of a tool name, title-cased.
fn title_case(word: &str) -> String {
    let mut c = word.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
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

/// `cd /Users/x/sources/hotl && cargo test` → `hotl ❯ cargo test`. Only `&&`
/// is elided: a `;` chain runs the tail whether or not the `cd` worked, which
/// is a different claim about what happened.
fn bash_body(body: &str) -> String {
    let Some((path, tail)) = body.strip_prefix("cd ").and_then(split_cd_path) else {
        return body.to_string();
    };
    let Some(command) = tail.strip_prefix("&&") else {
        return body.to_string();
    };
    let command = command.trim_start();
    if command.is_empty() {
        return body.to_string();
    }
    format!("{} ❯ {command}", dir_label(path))
}

/// The (possibly quoted) directory a `cd` names, and whatever follows it. An
/// unterminated quote is not a path — the summary stays verbatim.
fn split_cd_path(rest: &str) -> Option<(&str, &str)> {
    let rest = rest.trim_start();
    let first = rest.chars().next()?;
    if first == '\'' || first == '"' {
        let close = 1 + rest[1..].find(first)?;
        return Some((&rest[1..close], rest[close + 1..].trim_start()));
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some((&rest[..end], rest[end..].trim_start()))
}

/// The last component of a path, with `/` and a bare `..` kept as themselves —
/// a label, not a resolved directory.
fn dir_label(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    match trimmed.rsplit('/').next() {
        Some(last) if !last.is_empty() => last.to_string(),
        _ => trimmed.to_string(),
    }
}

/// Marker glyph + color for a tool status; running wears the wanderer frame
/// the card's own ticks select. Success is quiet — an arrow in the muted role,
/// not a tick in ink — so only failure and denial catch the eye (0061 T4).
fn status_glyph(status: &ToolStatus, ticks: u64, p: &Palette) -> (&'static str, Color) {
    match status {
        ToolStatus::Running | ToolStatus::AutoAllowed { .. } => {
            (WORKING_FRAMES[marker_frame(ticks)], p.active)
        }
        // Hollow and faint: parked, not working (0061 T24).
        ToolStatus::Queued { .. } => ("○", p.faint),
        ToolStatus::Done => (SETTLED_OK_GLYPH, p.muted),
        ToolStatus::Failed => ("✗", p.blocked),
        ToolStatus::Denied => ("⊘", p.blocked),
    }
}

/// The spawn card `selected_agent` names, when it still exists — a dangling
/// id renders the main transcript instead.
fn selected_spawn(state: &State) -> Option<&TranscriptItem> {
    let want = state.selected_agent.as_deref()?;
    state.transcript.iter().find(|i| {
        matches!(i, TranscriptItem::Tool { id, name, .. }
            if crate::app::is_agent_card(name) && id == want)
    })
}

/// The agent band (0039, navigable since 0043): a `main` row plus a
/// windowed row per running spawn. `●` = shown stream, band bg = cursor —
/// two independent things that usually coincide.
fn render_selector(state: &State, p: &Palette, frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }
    let spawns = state.band_spawns();
    let position = |want: &str| {
        spawns
            .iter()
            .position(|i| matches!(i, TranscriptItem::Tool { id, .. } if id == want))
            .map(|i| i + 1)
    };
    let sel_idx = state
        .selected_agent
        .as_deref()
        .and_then(position)
        .unwrap_or(0);
    let cur_idx = match &state.band_cursor {
        Some(BandRow::Main) => Some(0),
        Some(BandRow::Spawn(id)) => position(id),
        None => None,
    };
    let row_style = |on: bool| {
        if on {
            Style::new().fg(p.accent)
        } else {
            Style::new().fg(p.muted)
        }
    };
    let styled = |text: String, on: bool, idx: usize| {
        let mut line = Line::styled(text, row_style(on));
        if cur_idx == Some(idx) {
            line.style = line.style.patch(Style::new().bg(p.band));
        }
        line
    };
    let mut lines = vec![styled(
        format!("{} main", if sel_idx == 0 { "●" } else { "○" }),
        sel_idx == 0,
        0,
    )];
    // The window follows the cursor (else the selection); the rest overflow
    // to a count.
    let focus = cur_idx.unwrap_or(sel_idx);
    let start = if focus == 0 {
        0
    } else {
        (focus - 1).saturating_sub(SELECTOR_MAX_SPAWNS - 1)
    };
    let end = (start + SELECTOR_MAX_SPAWNS).min(spawns.len());
    for (i, item) in spawns.iter().enumerate().take(end).skip(start) {
        let TranscriptItem::Tool {
            summary,
            status,
            ticks,
            children,
            ..
        } = item
        else {
            continue;
        };
        let on = sel_idx == i + 1;
        let (glyph, _) = status_glyph(status, *ticks, p);
        let mut text = format!(
            "{} {glyph} {summary} · {}s",
            if on { "●" } else { "○" },
            ticks / anim::TICK_HZ
        );
        if !children.is_empty() {
            text.push_str(&format!(" · {}", call_count(children.len())));
        }
        lines.push(styled(
            text.chars().take(area.width as usize).collect::<String>(),
            on,
            i + 1,
        ));
    }
    if spawns.len() > SELECTOR_MAX_SPAWNS {
        lines.push(Line::styled(
            format!("  … +{} more", spawns.len() - (end - start)),
            Style::new().fg(p.faint),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The drill-in (0039 D7): the selected spawn's child stream, full-rect and
/// cache-free — the content is one card's children (bounded), and the tick
/// stamps it reads stay OUTSIDE the fingerprint invariant on purpose (no
/// `item_block` ever sees them). Wrapped behind a muted spine so
/// `text_col = gutter + 2` holds for drag-copy.
fn render_agent_stream(
    state: &State,
    item: &TranscriptItem,
    p: &Palette,
    frame: &mut Frame,
    area: Rect,
) {
    let TranscriptItem::Tool {
        summary,
        status,
        ticks,
        children,
        child_text,
        ..
    } = item
    else {
        return;
    };
    let running = matches!(status, ToolStatus::Running | ToolStatus::AutoAllowed { .. });
    let (glyph, color) = status_glyph(status, *ticks, p);
    let mut content = vec![
        Line::from(vec![
            Span::styled(format!("{glyph} "), Style::new().fg(color).bold()),
            Span::styled(summary.clone(), Style::new().fg(p.ink).bold()),
            Span::styled(
                format!(
                    " · {}s · {} · esc back",
                    ticks / anim::TICK_HZ,
                    if running { "live" } else { "settled" }
                ),
                Style::new().fg(p.muted).bold(),
            ),
        ]),
        Line::raw(""),
    ];
    for c in children {
        let (cg, cc) = match c.ok {
            None => (WORKING_FRAMES[marker_frame(*ticks)], p.active),
            Some(true) => ("✓", p.idle),
            Some(false) => ("✗", p.blocked),
        };
        let mut spans = vec![
            Span::styled(format!("{cg} {}", c.name), Style::new().fg(cc)),
            Span::styled(format!("  {}", c.summary), Style::new().fg(p.ink)),
        ];
        // Per-call duration from the parent-tick stamps. A running child
        // reads on the parent's live clock (0061 T29) — it used to show
        // nothing at all, which is where a stuck child hides.
        let end = c.settled_at.unwrap_or(*ticks);
        spans.push(Span::styled(
            format!(" · {}s", end.saturating_sub(c.started_at) / anim::TICK_HZ),
            Style::new().fg(p.muted),
        ));
        if let Some(n) = c.tokens {
            spans.push(Span::styled(
                format!(" · {} tok", crate::app::tok(n)),
                Style::new().fg(p.muted),
            ));
        }
        content.push(Line::from(spans));
    }
    // The child's own words (0058 T2), after its calls — this is the whole
    // reason to drill in on a running child.
    if !child_text.is_empty() {
        content.push(Line::raw(""));
        for line in child_text.lines() {
            content.push(Line::styled(line.to_string(), Style::new().fg(p.muted)));
        }
    }
    let gutter = state.density.gutter();
    let inner = (area.width as usize).saturating_sub(gutter + 2).max(1);
    let spine = Spine {
        indent: 0,
        marker: "·",
        cont: " ",
        marker_style: Style::new().fg(p.muted),
        cont_style: Style::new(),
    };
    let mut rows: Vec<Line> = Vec::new();
    for cl in &content {
        for wl in wrap::line(cl, inner) {
            let first = rows.is_empty();
            rows.push(spine.wrap(wl, gutter, first));
        }
    }
    // Follow the tail unless scrolled back; a stale offset clamps.
    let height = area.height as usize;
    let total = rows.len();
    let skip = match state.agent_scroll {
        None => total.saturating_sub(height),
        Some(o) => o.min(total.saturating_sub(1)),
    };
    // Anchored to the strip like the main transcript (0049 T4).
    let pad = height.saturating_sub(total);
    let mut visible: Vec<Line> = (0..pad).map(|_| Line::raw("")).collect();
    visible.extend(rows.into_iter().skip(skip).take(height - pad));
    frame.render_widget(Paragraph::new(visible), area);
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
    // The wave is per-column color, so it cannot ride the paragraph's single
    // style: one span per column, each carrying only a foreground so the band
    // background still comes from `style` below.
    let mut spans: Vec<Span> = anim::snake_for(state)
        .chars()
        .zip(anim::snake_ramp(&state.phase, p))
        .map(|(c, color)| Span::styled(c.to_string(), Style::new().fg(color)))
        .collect();
    // Three zones on one row (0049 T1): the snake, the left text, and the
    // chip cluster at the right edge. Width runs out by rank — fold, then
    // drop, then elide the text — so a chip is never painted over a word.
    let (segs, chips) = fit_strip(
        anim::strip_segments(state),
        strip_chips(state, p),
        anim::WIDTH,
        area.width as usize,
    );
    // One span per segment: a `Blocked` one is painted loud while the rest
    // take the paragraph's own style (0061 T19).
    for (i, seg) in segs.iter().enumerate() {
        let lead = if i == 0 { " " } else { " · " };
        let text = format!("{lead}{}", seg.text);
        spans.push(match seg.tone {
            anim::Tone::Blocked => Span::styled(text, Style::new().fg(p.blocked).bold()),
            anim::Tone::Plain => Span::raw(text),
        });
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(style), area);
    let mut x = area.right();
    for chip in &chips {
        let w = chip.text.chars().count() as u16;
        x = x.saturating_sub(w);
        let rect = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        frame.render_widget(Paragraph::new(chip.text.as_str()).style(chip.style), rect);
    }
}

/// A right-zone chip: text with its own style, plus a drop rank.
struct Chip {
    text: String,
    style: Style,
    rank: u8,
}

/// The name chip's text cap: a long name truncates before it ranks, so it
/// still fits at 80 columns instead of dropping.
const NAME_CHIP_MAX: usize = 40;

/// The name chip's rank — between usage detail and the model name.
const RANK_NAME_CHIP: u8 = 4;

/// The strip's right zone, right-to-left: session name, mode, context
/// share, flag count — the flags nearest the text. Only the name may drop.
fn strip_chips(state: &State, p: &Palette) -> Vec<Chip> {
    let mut chips = Vec::new();
    // The Claude-style badge just above the input.
    if let Some(name) = &state.session_name {
        let mut label: String = name.chars().take(NAME_CHIP_MAX).collect();
        if label.chars().count() < name.chars().count() {
            label.push('…');
        }
        chips.push(Chip {
            text: format!(" {label} "),
            style: Style::new().fg(p.band).bg(p.accent).bold(),
            rank: RANK_NAME_CHIP,
        });
    }
    // Always drawn: silence used to mean "ask", but a narrow terminal
    // dropped the chip too, so absence was ambiguous — and the mode it
    // implied was wrong (`hotl setup` writes `mode = "bypass"`, evaluation
    // §5.7). A supervision tool states its posture; it does not imply it
    // by omission.
    // INVARIANT: every mode renders its own name. Enforced by
    // `the_mode_badge_is_always_drawn`. The one badge-less state is
    // pre-open (0033 Task 8b, `mode` empty): no session exists yet, and
    // rendering *no* mode is the only honest option — never a guessed one.
    if !state.mode.is_empty() {
        chips.push(Chip {
            text: mode_chip_text(state),
            style: mode_chip_style(state, p),
            rank: anim::KEEP,
        });
    }
    // The context share (0040): the last turn's resident context, or the
    // at-open estimate before any turn completes — a resumed session
    // inherits real fullness, a fresh seed occupies tokens.
    if let Some(pct) = state
        .live_context
        .or(state.open_context)
        .and_then(|l| crate::app::ctx_pct(l, state.context_window))
    {
        chips.push(Chip {
            text: format!(" {pct}% "),
            style: Style::new().fg(p.muted).bg(p.band),
            rank: anim::KEEP,
        });
    }
    // The spend meter (0059 T5): only once a threshold has been crossed —
    // a budget nobody is near is not news, and the strip has one line.
    if let Some((used, cap)) = state.budget.filter(|(_, cap)| *cap > 0.0) {
        // Amber past 80%: the same "you are close" signal the notice carries.
        let hot = used >= 0.8 * cap;
        chips.push(Chip {
            text: format!(" ${used:.2}/${cap:.0} "),
            style: match hot {
                true => Style::new().fg(p.band).bg(p.accent).bold(),
                false => Style::new().fg(p.muted).bg(p.band),
            },
            rank: anim::KEEP,
        });
    }
    // Flag chip (0036): how many calls ran (or were refused) on a ⚑ notice
    // instead of an ask. A running count, never cleared mid-session, so an
    // unattended run's flags survive scrollback.
    if state.flag_count > 0 {
        chips.push(Chip {
            text: format!(" ⚑ {} ", state.flag_count),
            style: Style::new().fg(p.band).bg(p.blocked).bold(),
            rank: anim::KEEP,
        });
    }
    chips
}

/// `plan · bypass` — both posture axes on one chip: they are independent,
/// and a badge showing only the mode would hide half the posture.
fn mode_chip_text(state: &State) -> String {
    if state.plan {
        format!(" plan · {} ", state.mode)
    } else {
        format!(" {} ", state.mode)
    }
}

/// Unattended postures wear the blocked color: nobody is being consulted
/// on this session's tool calls. Plan outranks that — it is the posture the
/// user deliberately chose.
fn mode_chip_style(state: &State, p: &Palette) -> Style {
    if state.plan {
        Style::new().fg(p.band).bg(p.accent).bold()
    } else {
        match state.mode.as_str() {
            "bypass" | "dontask" => Style::new().fg(p.band).bg(p.blocked).bold(),
            _ => Style::new().fg(p.muted).bg(p.band),
        }
    }
}

/// Fold, then drop, by rank until the left text and the chips fit `width`
/// (each chip carries its own cell of padding, so no further gap is
/// reserved); as a last resort elide the left text at a ` · ` boundary.
/// Returns (surviving segments, surviving chips) — segments rather than one
/// string so the view can paint a `Blocked` one loud (0061 T19); their joined
/// text is exactly what the old string was.
fn fit_strip(
    mut segs: Vec<anim::Segment>,
    mut chips: Vec<Chip>,
    snake_w: usize,
    width: usize,
) -> (Vec<anim::Segment>, Vec<Chip>) {
    let join = anim::join;
    let chips_w = |chips: &[Chip]| chips.iter().map(|c| c.text.chars().count()).sum::<usize>();
    let fits = |segs: &[anim::Segment], chips: &[Chip]| {
        snake_w + 1 + join(segs).chars().count() + chips_w(chips) <= width
    };
    while !fits(&segs, &chips) {
        // Lowest rank first; a foldable segment folds before anything drops.
        let seg = segs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.rank != anim::KEEP)
            .min_by_key(|(_, s)| s.rank)
            .map(|(i, _)| i);
        let chip = chips
            .iter()
            .enumerate()
            .filter(|(_, c)| c.rank != anim::KEEP)
            .min_by_key(|(_, c)| c.rank)
            .map(|(i, _)| i);
        match (seg, chip) {
            (Some(i), c) if c.is_none_or(|j| segs[i].rank <= chips[j].rank) => {
                if let Some(short) = segs[i].short.take() {
                    segs[i].text = short;
                } else {
                    segs.remove(i);
                }
            }
            (_, Some(j)) => {
                chips.remove(j);
            }
            // Nothing droppable is left; the elision below takes over.
            _ => break,
        }
    }
    // The elision keeps whole segments where it can and cuts the last one
    // hard, so the joined text is exactly what `elide_at_separator` would
    // have produced from the old single string.
    let room = width.saturating_sub(snake_w + 1 + chips_w(&chips));
    let full = join(&segs);
    if full.chars().count() > room {
        let want: Vec<char> = elide_at_separator(&full, room).chars().collect();
        let mut out: Vec<anim::Segment> = Vec::new();
        let mut used = 0usize;
        for mut seg in segs {
            let sep = if out.is_empty() { 0 } else { " · ".len() };
            if used + sep >= want.len() {
                break;
            }
            used += sep;
            let left = want.len() - used;
            let len = seg.text.chars().count();
            let take = len.min(left);
            if take < len {
                seg.text = want[used..used + take].iter().collect();
            }
            used += take;
            out.push(seg);
            if take < len {
                break;
            }
        }
        // Whatever the elision added past the last kept segment (the ` …`).
        if used < want.len() {
            let tail: String = want[used..].iter().collect();
            match out.last_mut() {
                Some(last) => last.text.push_str(&tail),
                None => out.push(anim::Segment::rank(tail, anim::KEEP)),
            }
        }
        segs = out;
    }
    (segs, chips)
}

/// Cut at the last ` · ` that leaves room for `…`; a single overlong token
/// is cut hard.
fn elide_at_separator(text: &str, room: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= room {
        return text.to_string();
    }
    if room == 0 {
        return String::new();
    }
    // `head …` — the head ends where a separator began.
    let cut = text
        .match_indices(" · ")
        .map(|(at, _)| text[..at].chars().count())
        .filter(|&n| n + 2 <= room)
        .last();
    match cut {
        Some(n) => format!("{} …", chars[..n].iter().collect::<String>()),
        None => format!("{}…", chars[..room - 1].iter().collect::<String>()),
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
/// the transcript keeps its 3-row minimum. `area` is the whole frame.
fn input_height(state: &State, area: Rect) -> u16 {
    // Gutter, two border cells, one cell of padding.
    let width = (area.width as usize)
        .saturating_sub(state.density.gutter() + 3)
        .max(1);
    let (rows, _) = input_rows(&state.editor.text(), state.editor.cursor(), width);
    let body = rows.len().clamp(1, draft_cap(area)) as u16;
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
    // One cell of padding, so the text lands at `gutter + 2` — the
    // transcript's text column (0049 T3).
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(1),
        ..inner
    };
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
        // The modal's own key line names y/n/s (0049 T6); the row keeps
        // the keys that leave it.
        (Phase::WaitingAsk { .. }, ..) => "esc interrupt · ctrl-c",
        (Phase::WaitingQuestion { .. }, ..) => {
            "↑↓ or 1-9 pick · enter choose · type for free text · esc clear/interrupt"
        }
        (Phase::WaitingEgress { .. }, ..) => {
            "y allow this host for the session · n deny · esc interrupt · ctrl-c"
        }
        _ if state.editor.search_prompt().is_some() => {
            "type to search · ctrl-r older · enter accept · esc cancel"
        }
        _ if state.completion.is_some() => "↑↓ pick · tab complete · enter run · esc dismiss",
        _ if copied.is_some() => copied.as_deref().unwrap_or_default(),
        // 0043: the engaged band cursor (requires an empty buffer, so it can
        // never collide with the popup's hint above).
        (_, true, Mode::Normal) if state.band_cursor.is_some() => {
            "j/k move · enter open · esc back"
        }
        _ if state.band_cursor.is_some() => "↑↓ move · enter open · esc back",
        _ if selected_spawn(state).is_some() => "↑↓ agents · esc back to main · pgup/pgdn scroll",
        // 0061 T20: the first Esc is spent; say what the second one does.
        // Below the modal hints, whose handlers own the keyboard outright.
        _ if state.interrupt_sent => "esc again takes control back · ctrl-c quits",
        // Phase-aware (0049 T8): `esc interrupt` only while something runs.
        (Phase::Idle, true, Mode::Normal) => "i insert · j/k scroll · ? help · ctrl-g editor",
        (_, true, Mode::Normal) => "esc interrupt · i insert · ? help",
        (Phase::Idle, ..) => "? help · / commands · ↑↓ history · ctrl-r search · ctrl-g editor",
        _ => "esc interrupt · enter steer · ? help",
    };
    let hint = fit_hint(hint, area.width as usize);
    frame.render_widget(Paragraph::new(hint).style(Style::new().fg(p.faint)), area);
}

/// Cut a hint at the last ` · ` that fits `width`, so a narrow terminal
/// drops whole keys rather than half of one.
fn fit_hint(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let cut = text
        .match_indices(" · ")
        .map(|(at, _)| at)
        .filter(|&at| text[..at].chars().count() <= width)
        .last();
    match cut {
        Some(at) => text[..at].to_string(),
        None => text.chars().take(width).collect(),
    }
}

/// How a modal row meets the edge: prose folds, code and commands clip.
#[derive(Clone, Copy)]
enum Fold {
    Prose,
    Clip,
}

/// A modal body: rows with their fold rule.
type Body<'a> = Vec<(Line<'a>, Fold)>;

/// The widest an ask/question/egress modal grows, as a share of the frame.
const MODAL_MAX_PCT: u16 = 90;

/// A modal's frame: its title, border style, and how wide it may grow.
struct Chrome<'a> {
    title: &'a str,
    border: Style,
    max_pct: u16,
}

/// Content width plus padding, clamped to 60–`max_pct`% of `over`; centered.
fn modal_rect(over: Rect, content_w: u16, rows: u16, max_pct: u16) -> Rect {
    let lo = (u32::from(over.width) * 60 / 100) as u16;
    let hi = (u32::from(over.width) * u32::from(max_pct) / 100) as u16;
    let width = (content_w + 4)
        .clamp(lo.max(10), hi.max(10))
        .min(over.width);
    let height = rows.min(over.height);
    Rect {
        x: over.x + (over.width - width) / 2,
        y: over.y + (over.height - height) / 2,
        width,
        height,
    }
}

/// Prose wrap with a hanging indent: a row that starts with spaces (an
/// option's description, a help continuation) keeps that indent on every
/// row it wraps onto, so the text column stays a column.
fn wrap_hanging<'a>(line: &Line<'a>, w: usize) -> Vec<Line<'a>> {
    let indent = line
        .spans
        .first()
        .map(|s| s.content.chars().take_while(|c| *c == ' ').count())
        .unwrap_or(0);
    // Too little room for a hanging column: wrap flat.
    if indent == 0 || indent + 8 > w {
        return wrap::line(line, w);
    }
    let mut stripped = line.clone();
    let first = &mut stripped.spans[0];
    *first = Span::styled(first.content[indent..].to_string(), first.style);
    wrap::line(&stripped, w - indent)
        .into_iter()
        .map(|mut wl| {
            wl.spans.insert(0, Span::raw(" ".repeat(indent)));
            wl
        })
        .collect()
}

/// A row cut hard at `w` cells with `…`, keeping the line's own style.
fn clip_line<'a>(line: &Line<'a>, w: usize) -> Line<'a> {
    if line.width() <= w {
        return line.clone();
    }
    use unicode_width::UnicodeWidthChar;
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    Line::styled(out, line.style)
}

/// Draw a bordered modal (0049 T6): one cell of padding each side, prose
/// rows wrapped to the inner width, clip rows cut with `…`, and a window of
/// `scroll` rows when the body is taller than the frame — the last row then
/// reads `… +N more · pgdn`.
fn draw_modal(
    frame: &mut Frame,
    p: &Palette,
    over: Rect,
    chrome: Chrome,
    body: &[(Line, Fold)],
    scroll: usize,
) {
    let content_w = body.iter().map(|(l, _)| l.width()).max().unwrap_or(0) as u16;
    // Width first, so the rows can be laid out; the height follows them.
    let inner_w = modal_rect(over, content_w, 0, chrome.max_pct)
        .width
        .saturating_sub(4)
        .max(1) as usize;
    let mut rows: Vec<Line> = Vec::new();
    for (line, fold) in body {
        match fold {
            Fold::Prose => rows.extend(wrap_hanging(line, inner_w)),
            Fold::Clip => rows.push(clip_line(line, inner_w)),
        }
    }
    let avail = over.height.saturating_sub(2) as usize;
    if rows.len() > avail && avail > 0 {
        let top = scroll.min(rows.len() - avail);
        let below = rows.len() - top - avail;
        rows = rows.into_iter().skip(top).take(avail).collect();
        if below > 0 {
            rows.pop();
            rows.push(Line::styled(
                format!("… +{} more · pgdn", below + 1),
                Style::new().fg(p.faint),
            ));
        }
    }
    let area = modal_rect(over, content_w, rows.len() as u16 + 2, chrome.max_pct);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(chrome.title)
        .border_style(chrome.border);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    frame.render_widget(Paragraph::new(rows), inner);
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
    let mut body: Body = vec![(
        Line::styled(summary.clone(), Style::new().fg(p.ink).bold()),
        Fold::Prose,
    )];
    if let Some(why) = protected_why {
        body.push((
            Line::styled(format!("⚠ {why}"), Style::new().fg(p.blocked).bold()),
            Fold::Prose,
        ));
    }
    // The proposed change, between the summary and the y/n line — approving a
    // write without seeing it is the gap this closes. Empty until the engine's
    // ask carries the tool input (RQ-2), and an empty diff must render exactly
    // as the card did before. Diff rows clip, never wrap: a wrapped diff
    // line reads as two lines that were never in the file.
    if !diff.is_empty() {
        body.push((Line::raw(""), Fold::Prose));
        for l in diff {
            let (prefix, style) = match l.op {
                DiffOp::Add => ("+ ", Style::new().fg(p.idle)),
                DiffOp::Del => ("- ", Style::new().fg(p.blocked)),
                DiffOp::Ctx => ("  ", Style::new().fg(p.muted)),
                DiffOp::Trailer => ("  ", Style::new().fg(p.faint).dim()),
            };
            body.push((
                Line::styled(format!("{prefix}{}", l.text), style),
                Fold::Clip,
            ));
        }
    }
    body.push((Line::raw(""), Fold::Prose));
    if *denying {
        body.push((
            Line::styled(format!("deny reason: {input}▏"), Style::new().fg(p.ink)),
            Fold::Prose,
        ));
    } else {
        // Plan 0022: `s` is offered only where it does something — a bash ask
        // whose label does not already say the credential reads are open.
        // An option that is a no-op is worse than no option.
        body.push((
            Line::styled(
                if crate::app::secret_read_grant_applies(summary) {
                    "y allow · s allow + credential reads (this command only) · n deny"
                } else {
                    "y allow · n deny · type a reason after n"
                },
                Style::new().fg(p.faint),
            ),
            Fold::Prose,
        ));
    }
    draw_modal(
        frame,
        p,
        over,
        Chrome {
            title: " waiting on you ",
            border: Style::new().fg(p.blocked),
            max_pct: MODAL_MAX_PCT,
        },
        &body,
        state.modal_scroll,
    );
}

/// `ask_user`'s option-picker modal (tier-1 gap #4) — generalizes
/// `render_ask`'s y/n card to N labelled options (2-4, per the tool's own
/// validation) plus free text. Not a permission card, and dressed unlike
/// one (0047 D3): its own title and the accent border, so blocked color +
/// "waiting on you" stay exclusive to prompts where a key grants authority.
/// The strip's blocked phase text is untouched — the turn *is* halted.
fn render_question(state: &State, p: &Palette, frame: &mut Frame, over: Rect) {
    let Phase::WaitingQuestion {
        header,
        prompt,
        options,
        input,
        selected,
        ..
    } = &state.phase
    else {
        return;
    };
    let mut body: Body = vec![
        (
            Line::styled(header.clone(), Style::new().fg(p.ink).bold()),
            Fold::Prose,
        ),
        (
            Line::styled(prompt.clone(), Style::new().fg(p.ink)),
            Fold::Prose,
        ),
        (Line::raw(""), Fold::Prose),
    ];
    // Options as a list with a cursor (0049 T6, LD5): `› n  label`, the
    // description muted on its own row, aligned under the label.
    for (i, opt) in options.iter().enumerate() {
        let on = i == *selected;
        let label = if on {
            Style::new().fg(p.ink).bold()
        } else {
            Style::new().fg(p.ink)
        };
        body.push((
            Line::from(vec![
                Span::styled(
                    if on { "› " } else { "  " },
                    Style::new().fg(p.accent).bold(),
                ),
                Span::styled(format!("{}  ", i + 1), Style::new().fg(p.muted)),
                Span::styled(opt.label.clone(), label),
            ]),
            Fold::Prose,
        ));
        if let Some(desc) = &opt.description {
            body.push((
                Line::styled(format!("     {desc}"), Style::new().fg(p.muted)),
                Fold::Prose,
            ));
        }
    }
    body.push((Line::raw(""), Fold::Prose));
    if input.is_empty() {
        body.push((
            Line::styled(
                format!(
                    "↑↓ or 1-{} pick · enter choose · type for free text",
                    options.len()
                ),
                Style::new().fg(p.faint),
            ),
            Fold::Prose,
        ));
    } else {
        body.push((
            Line::styled(format!("free text: {input}▏"), Style::new().fg(p.ink)),
            Fold::Prose,
        ));
    }
    draw_modal(
        frame,
        p,
        over,
        Chrome {
            title: " a question for you ",
            border: Style::new().fg(p.accent),
            max_pct: MODAL_MAX_PCT,
        },
        &body,
        state.modal_scroll,
    );
}

/// Columns the slash-command list wraps at inside the help table.
const HELP_SLASH_COLS: usize = 58;

/// The help table's label column: the widest label plus one space.
const HELP_LABEL_COLS: usize = 11;

/// Help content assembled from the live command table and the mode flags —
/// never a hand-maintained list (it drifted: /goal and /workflows were
/// missing, and a rebind made two lines false). `(label, text)` rows, the
/// label empty on continuation rows (0049 T7): a table 78 cells wide that
/// fits 80×24 with vim off and scrolls when it must.
pub(crate) fn help_lines(state: &State) -> Vec<(&'static str, String)> {
    let row = |label: &'static str, text: &str| (label, text.to_string());
    let mut rows = vec![
        row(
            "composer",
            "enter send · shift-enter newline · ctrl-v paste image/text",
        ),
        row(
            "",
            "← → home end · ctrl-a/e line ends · alt-←/→ by word · del",
        ),
        row(
            "",
            "ctrl-k/u kill to end/start · ctrl-w word back · ctrl-g $EDITOR",
        ),
    ];
    if state.vim_mode {
        rows.extend([
            row(
                "vim",
                "esc normal · i a I A o O insert · h l 0 $ w b e (+counts)",
            ),
            row("", "d c y operators · dd cc yy x p u · :e $EDITOR"),
        ]);
    }
    rows.extend([
        row("history", "↑ ↓ recall (prefix-aware) · ctrl-r search"),
        row(
            "commands",
            "/ opens completion · ↑↓ pick · tab complete · enter run",
        ),
        row("transcript", "pgup pgdn · ctrl-home/end · wheel"),
        row("", "ctrl-t thinking · ctrl-o tool cards"),
        row(
            "",
            "drag to copy · shift-drag for the terminal's own select",
        ),
        row(
            "agents",
            if state.vim_mode {
                "↑↓ (j/k normal) band · enter open · esc back"
            } else {
                "↑↓ band · enter open · esc back"
            },
        ),
        row("interrupt", "esc (empty) · esc again insists · ctrl-c quit"),
        row(
            "asks",
            "y allow · n deny + reason · s allow + credential reads",
        ),
    ]);
    let commands: Vec<String> = crate::complete::builtins()
        .into_iter()
        .map(|c| format!("/{}", c.name))
        .collect();
    for (i, line) in pack(&commands, HELP_SLASH_COLS).into_iter().enumerate() {
        rows.push((if i == 0 { "slash" } else { "" }, line));
    }
    rows.push(row("", ""));
    rows.push(row("", "any key closes this help"));
    rows
}

/// Greedy word-wrap of `words` into lines no wider than `cols` chars.
fn pack(words: &[String], cols: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in words {
        match out.last_mut() {
            Some(line) if line.chars().count() + 1 + w.chars().count() <= cols => {
                line.push(' ');
                line.push_str(w);
            }
            _ => out.push(w.clone()),
        }
    }
    out
}

/// The help table over the whole frame (0049 T7): label column muted, text
/// in ink, sized from the widest row; `modal_scroll` is the window.
fn render_help(state: &State, p: &Palette, frame: &mut Frame, over: Rect) {
    let body: Body = help_lines(state)
        .into_iter()
        .map(|(label, text)| {
            (
                Line::from(vec![
                    Span::styled(
                        format!("{label:<HELP_LABEL_COLS$}"),
                        Style::new().fg(p.muted),
                    ),
                    Span::styled(text, Style::new().fg(p.ink)),
                ]),
                Fold::Prose,
            )
        })
        .collect();
    draw_modal(
        frame,
        p,
        over,
        Chrome {
            title: " keys ",
            border: Style::new().fg(p.accent),
            // Wider than the other modals: the table is 78 cells by design.
            max_pct: 100,
        },
        &body,
        state.modal_scroll,
    );
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
    let body: Body = vec![
        (
            Line::styled(
                format!("reaching \"{host}\" was not in the approved command"),
                Style::new().fg(p.ink).bold(),
            ),
            Fold::Prose,
        ),
        (
            Line::styled(
                "this host is not in [network].allow".to_string(),
                Style::new().fg(p.muted),
            ),
            Fold::Prose,
        ),
        (Line::raw(""), Fold::Prose),
        (
            Line::styled(
                "y allow for this session · n deny",
                Style::new().fg(p.faint),
            ),
            Fold::Prose,
        ),
    ];
    draw_modal(
        frame,
        p,
        over,
        Chrome {
            title: " network egress ",
            border: Style::new().fg(p.blocked),
            max_pct: MODAL_MAX_PCT,
        },
        &body,
        state.modal_scroll,
    );
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
    let area = above_input(
        over,
        width,
        lines.len() as u16 + 2,
        state.density.gutter() as u16,
    );
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

/// A rect `width`×`height`, pinned to `over`'s bottom-left corner, `gutter`
/// cells in — so the popup's left border aligns with the box's.
fn above_input(over: Rect, width: u16, height: u16, gutter: u16) -> Rect {
    let gutter = gutter.min(over.width);
    let width = width.max(10).min(over.width - gutter);
    let height = height.min(over.height);
    Rect {
        x: over.x + gutter,
        y: over.y + over.height - height,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::State;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use hotl_tools::todo::{Todo, TodoStatus};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    type Buffer = ratatui::buffer::Buffer;

    /// One frame at `w`×`h` through `cache`, as the raw buffer.
    fn render_at(state: &State, cache: &mut TranscriptCache, w: u16, h: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| view(state, &Palette::default(), cache, f))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn rows_of(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect()
            })
            .collect()
    }

    /// Bottom anchoring (0049 T4) pads a short transcript from the top. So
    /// the tests that index transcript rows from 0 keep their meaning, the
    /// region's leading blank run is rotated to its end: a leading blank run
    /// in the transcript is always anchoring padding — the first row of any
    /// item carries a spine glyph, and separators sit between items, never
    /// before the first. Skipped while an overlay (modal, popup, help) is
    /// drawn over the region; the anchoring tests use `draw_raw`.
    fn normalize(state: &State, buffer: &mut Buffer) {
        let overlay = state.help_open
            || state.completion.is_some()
            || matches!(
                state.phase,
                Phase::WaitingAsk { .. }
                    | Phase::WaitingQuestion { .. }
                    | Phase::WaitingEgress { .. }
            );
        if overlay {
            return;
        }
        let region = regions(state, buffer.area)[0];
        let blank = |y: u16| {
            (region.x..region.right())
                .all(|x| buffer.cell((x, y)).unwrap().symbol().trim().is_empty())
        };
        let lead = (region.y..region.bottom())
            .take_while(|&y| blank(y))
            .count();
        let height = region.height as usize;
        if lead == 0 || lead == height {
            return;
        }
        let rows: Vec<Vec<ratatui::buffer::Cell>> = (region.y..region.bottom())
            .map(|y| {
                (region.x..region.right())
                    .map(|x| buffer.cell((x, y)).unwrap().clone())
                    .collect()
            })
            .collect();
        for (i, y) in (region.y..region.bottom()).enumerate() {
            let src = &rows[(i + lead) % height];
            for (j, x) in (region.x..region.right()).enumerate() {
                *buffer.cell_mut((x, y)).unwrap() = src[j].clone();
            }
        }
    }

    /// The rows of a frame drawn at `w`×`h`, transcript normalized — `draw`
    /// at 80×24.
    fn draw_at(state: &State, w: u16, h: u16) -> Vec<String> {
        draw_cached_at(state, &mut TranscriptCache::default(), w, h)
    }

    /// The rows exactly as rendered — for the anchoring tests.
    fn draw_raw(state: &State, w: u16, h: u16) -> Vec<String> {
        rows_of(&render_at(state, &mut TranscriptCache::default(), w, h))
    }

    /// The 80×24 buffer, transcript normalized, for cell-level assertions.
    fn draw_buffer(state: &State) -> Buffer {
        let mut buffer = render_at(state, &mut TranscriptCache::default(), 80, 24);
        normalize(state, &mut buffer);
        buffer
    }

    /// Draw through one cache and return the rows, so a cached render that
    /// differs from a fresh one fails whatever assertion the caller makes.
    fn draw_cached(state: &State, cache: &mut TranscriptCache) -> Vec<String> {
        draw_cached_at(state, cache, 80, 24)
    }

    fn draw_cached_at(state: &State, cache: &mut TranscriptCache, w: u16, h: u16) -> Vec<String> {
        let mut buffer = render_at(state, cache, w, h);
        normalize(state, &mut buffer);
        rows_of(&buffer)
    }

    fn draw(state: &State) -> Vec<String> {
        draw_at(state, 80, 24)
    }

    /// A minimal tool card — 0039 gave `Tool` its `calls`/`children` vecs,
    /// and this keeps the next field from churning every literal again. The
    /// anchor call's settle state follows the status (D5).
    fn tool_item(
        id: &str,
        name: &str,
        summary: &str,
        status: ToolStatus,
        ticks: u64,
    ) -> TranscriptItem {
        let ok = match status {
            ToolStatus::Queued { .. } | ToolStatus::Running | ToolStatus::AutoAllowed { .. } => {
                None
            }
            ToolStatus::Done => Some(true),
            ToolStatus::Failed | ToolStatus::Denied => Some(false),
        };
        TranscriptItem::Tool {
            id: id.into(),
            name: name.into(),
            summary: summary.into(),
            status,
            ticks,
            calls: vec![crate::app::ToolCall {
                id: id.into(),
                ok,
                lines: None,
                bytes: None,
            }],
            children: Vec::new(),
            child_text: String::new(),
            progress: None,
        }
    }

    // 80×24 layout: transcript rows 0-18, strip 19, input 20-22, hint 23.
    const STRIP: usize = 19;
    const INPUT_TOP: usize = 20;
    const HINT: usize = 23;

    /// Column the `Comfortable` spine (gutter 2 + glyph + space) hands prose
    /// over at, and the transcript text used by the selection tests.
    const TEXT_COL: u16 = 4;
    const PROSE: &str = "alpha beta gamma";
    /// Where one prose row lands under bottom anchoring (0049 T4): just
    /// above the strip.
    const PROSE_ROW: u16 = (STRIP - 1) as u16;

    /// One assistant turn, so transcript row 0 is `"  ● alpha beta gamma"`.
    fn state_with_prose() -> State {
        let mut s = State::test_default();
        s.transcript = vec![TranscriptItem::Assistant { text: PROSE.into() }];
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

    /// The 80×24 buffer exactly as rendered: a drag is painted at screen
    /// cells, so these tests address the prose where anchoring put it.
    fn raw_buffer(state: &State) -> Buffer {
        render_at(state, &mut TranscriptCache::default(), 80, 24)
    }

    #[test]
    fn a_drag_highlights_exactly_the_cells_it_covers() {
        let mut s = state_with_prose();
        s.selection = Some(crate::select::Selection {
            anchor: (TEXT_COL, PROSE_ROW),
            head: (TEXT_COL + 4, PROSE_ROW),
        });
        let buffer = raw_buffer(&s);
        assert_eq!(
            reversed_rows(&buffer),
            vec![(PROSE_ROW, "alpha".to_string())],
            "only the dragged cells may reverse"
        );
    }

    #[test]
    fn what_is_highlighted_is_what_gets_copied() {
        // The feature's central invariant: the painted region and the scraped
        // text are read from the same buffer, so they cannot disagree.
        let mut s = state_with_prose();
        let sel = crate::select::Selection {
            anchor: (TEXT_COL, PROSE_ROW),
            head: (TEXT_COL + 9, PROSE_ROW),
        };
        s.selection = Some(sel);
        let buffer = raw_buffer(&s);
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
            anchor: (0, PROSE_ROW),
            head: (79, PROSE_ROW),
        };
        s.selection = Some(sel);
        assert_eq!(selection_text(&s, &raw_buffer(&s), &sel), PROSE);
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
        assert!(rows.iter().any(|r| r.contains("drag to copy")), "{rows:?}");
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
        let hint = draw(&s)[HINT].clone();
        assert!(hint.starts_with("esc interrupt"), "{hint:?}");
        assert!(!hint.contains("copied"), "{hint:?}");
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
                .join("\n")
                .into(),
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
        assert!(hint.starts_with("esc interrupt"), "got: {hint}");
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

    /// The snake at rest — what idle shows, and what these layout tests look
    /// for on the left of the strip.
    fn still() -> String {
        anim::at_rest()
    }

    #[test]
    fn idle_layout_shows_resting_wave_and_hint_row() {
        let rows = draw(&State::new(true, "m".into()));
        assert!(
            rows[STRIP].contains(&still()),
            "resting wave: {}",
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
                ..Default::default()
            },
            hotl_tools::todo::Todo {
                content: "wire the gate".into(),
                status: hotl_tools::todo::TodoStatus::InProgress,
                active_form: Some("wiring the gate".into()),
                ..Default::default()
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

    /// 0036: the idle strip carries the ⚑ flag count once any bypass call ran
    /// (or was refused) on a notice instead of an ask — absent at zero.
    #[test]
    fn strip_shows_the_flag_chip_at_idle() {
        let mut s = State::new(true, "m".into());
        assert!(
            !draw(&s)[STRIP].contains('⚑'),
            "no chip before the first flag"
        );
        s.flag_count = 3;
        let rows = draw(&s);
        assert!(rows[STRIP].contains("⚑ 3"), "{}", rows[STRIP]);
    }

    // ---- 0049 T1: strip zones and the drop order ----

    /// The strip row of a frame — the one row carrying the mode chip.
    /// Located by content, not index: T3 adds a gap row above the box on
    /// tall terminals.
    fn strip_row(rows: &[String], mode: &str) -> String {
        rows.iter()
            .find(|r| r.contains(mode))
            .cloned()
            .expect("a strip row")
    }

    /// The busy state every strip test starts from: a running bash, a
    /// four-item todo list with one in progress, a name, bypass mode.
    fn busy_strip_state() -> State {
        let mut s = State::new(false, "claude-opus-5".into());
        s.session_name = Some("retry-dedupe".into());
        s.mode = "bypass".into();
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 95,
        };
        let todo = |content: &str, status, active_form: Option<&str>| Todo {
            content: content.into(),
            status,
            active_form: active_form.map(str::to_string),
            ..Default::default()
        };
        s.todos = vec![
            todo("a", TodoStatus::Completed, None),
            todo("b", TodoStatus::Completed, None),
            todo("c", TodoStatus::InProgress, Some("running the suite")),
            todo("d", TodoStatus::Pending, None),
        ];
        s
    }

    #[test]
    fn the_strip_never_draws_a_chip_over_its_text() {
        let s = busy_strip_state();
        for (w, h) in [(80u16, 24u16), (60, 18), (120, 40)] {
            let row = strip_row(&draw_at(&s, w, h), "bypass");
            // Every ` · `-separated token on the left is a whole token: the
            // label is there in full or folded away, never cut.
            let left = row.split("bypass").next().unwrap();
            assert!(
                left.contains("running the suite") || !left.contains("running"),
                "{w}: cut mid-word: {row}"
            );
            assert!(row.contains("bash · 3s"), "{w}: phase survives: {row}");
            assert!(row.contains("2/4"), "{w}: todo count survives: {row}");
            assert!(row.contains("bypass"), "{w}: mode chip survives: {row}");
        }
    }

    #[test]
    fn the_todo_label_folds_to_its_count_at_60_cols() {
        let row = strip_row(&draw_at(&busy_strip_state(), 60, 18), "bypass");
        assert!(row.contains("2/4") && !row.contains("running"), "{row}");
        assert!(
            row.contains("retry-dedupe"),
            "the name still fits once the label folds: {row}"
        );
    }

    #[test]
    fn the_flag_chip_outranks_usage_detail_at_120_cols_idle() {
        let mut s = busy_strip_state();
        s.phase = Phase::Idle;
        s.usage_line = Some("12.4k in · 1.8k out · 9.1k cached · 71% hit · $0.42".into());
        s.flag_count = 2;
        s.plan = true;
        s.mode = "ask".into();
        let row = strip_row(&draw_at(&s, 120, 40), "plan · ask");
        assert!(row.contains("⚑ 2"), "flags: {row}");
        assert!(row.contains("plan · ask"), "mode: {row}");
    }

    #[test]
    fn esc_to_interrupt_left_the_strip() {
        let rows = draw(&busy_strip_state());
        assert!(!rows[STRIP].contains("esc"), "{}", rows[STRIP]);
    }

    #[test]
    fn the_ctx_chip_shows_open_context_before_the_first_turn() {
        let mut s = State::new(true, "m".into());
        s.open_context = Some(s.context_window / 8); // 12%
        let rows = draw(&s);
        assert!(rows[STRIP].contains(" 12% "), "{}", rows[STRIP]);
    }

    // ---- 0049 T3: the box on the grid ----

    #[test]
    fn the_box_sits_on_the_gutter_and_its_text_at_the_transcript_column() {
        let mut s = State::new(false, "m".into());
        for c in "hi".chars() {
            s.editor
                .handle(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let rows = draw(&s);
        assert!(
            rows[INPUT_TOP].starts_with("  ┌"),
            "border at the gutter: {:?}",
            rows[INPUT_TOP]
        );
        let body: Vec<char> = rows[INPUT_TOP + 1].chars().collect();
        assert_eq!(body[2], '│');
        assert_eq!(body[3], ' ', "one cell of padding");
        assert_eq!(body[4], 'h', "text at column 4 = TEXT_COL");
        assert_eq!(draw_cursor(&s), (TEXT_COL + 2, (INPUT_TOP + 1) as u16));
    }

    #[test]
    fn compact_density_puts_the_box_at_column_0() {
        let mut s = State::new(false, "m".into());
        s.density = hotl_theme::Density::Compact;
        assert!(draw(&s)[INPUT_TOP].starts_with('┌'));
    }

    #[test]
    fn a_gap_row_separates_strip_and_box_from_30_rows_up() {
        let s = State::new(false, "m".into());
        let tall = draw_at(&s, 80, 40);
        assert!(tall[34].contains(&still()), "strip at 34: {:?}", tall[34]);
        assert_eq!(tall[35].trim(), "", "gap row");
        assert!(tall[36].starts_with("  ┌"), "box at 36: {:?}", tall[36]);
        let short = draw(&s);
        assert!(short[INPUT_TOP].starts_with("  ┌"), "no gap at 24 rows");
    }

    #[test]
    fn the_draft_cap_scales_with_height() {
        let mut s = State::new(false, "m".into());
        for i in 0..30 {
            for c in format!("line {i}").chars() {
                s.editor
                    .handle(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            s.editor
                .handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        }
        let box_rows = |rows: &[String]| {
            rows.iter()
                .filter(|r| r.trim_start().starts_with('│'))
                .count()
        };
        assert_eq!(box_rows(&draw_at(&s, 80, 24)), 8, "24/3");
        assert_eq!(box_rows(&draw_at(&s, 80, 60)), 20, "60/3");
        assert_eq!(box_rows(&draw_at(&s, 80, 12)), 5, "floor");
    }

    #[test]
    fn the_completion_popup_aligns_with_the_box() {
        let mut s = State::new(false, "m".into());
        s.completion = Some(crate::complete::Completion {
            matches: vec![0, 1],
            selected: 0,
        });
        let rows = draw(&s);
        let popup = rows.iter().find(|r| r.contains("commands")).unwrap();
        assert!(popup.starts_with("  ┌"), "{popup:?}");
    }

    // ---- 0049 T8: small repairs ----

    #[test]
    fn the_hint_is_phase_aware() {
        let mut s = State::new(true, "m".into());
        let idle = draw(&s)[HINT].clone();
        assert!(!idle.contains("esc interrupt"), "{idle:?}");
        assert!(idle.contains("? help"), "{idle:?}");
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 0,
        };
        let busy = draw(&s)[HINT].clone();
        assert!(busy.starts_with("esc interrupt"), "{busy:?}");
    }

    #[test]
    fn the_hint_truncates_at_a_separator() {
        let s = State::new(false, "m".into());
        let rows = draw_at(&s, 60, 18);
        let hint = rows[17].trim_end().to_string();
        assert!(hint.ends_with("ctrl-r search"), "whole token: {hint:?}");
        assert!(!hint.ends_with('·'), "{hint:?}");
        assert_eq!(fit_hint("a · b · c", 5), "a · b");
        assert_eq!(fit_hint("abcdefgh", 4), "abcd");
    }

    #[test]
    fn a_denied_card_wears_a_one_cell_glyph() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![
            tool_item("t1", "write", "~/.ssh/config", ToolStatus::Denied, 0),
            tool_item("t2", "read", "a.rs", ToolStatus::Done, 0),
        ];
        let rows = draw(&s);
        // Cards sit one column in from prose (0061 T3).
        assert!(rows[0].starts_with("   ⊘ Wrote"), "{:?}", rows[0]);
        // Same column as its neighbour: the verb starts at TEXT_COL on both.
        assert_eq!(rows[0].find("Wrote"), rows[1].find("Read"));
    }

    /// A running card with progress, for the T25 renders.
    fn with_progress(ticks: u64, at_ticks: u64, tail: &str, lines: u64) -> TranscriptItem {
        let mut item = tool_item(
            "t1",
            "bash",
            "bash: cargo build",
            ToolStatus::Running,
            ticks,
        );
        if let TranscriptItem::Tool { progress, .. } = &mut item {
            *progress = Some(crate::app::ToolProgress {
                tail: tail.into(),
                lines,
                bytes: lines * 40,
                at_ticks,
            });
        }
        item
    }

    /// 0061 T25: the newest line the tool printed, on one faint row under the
    /// header, with the running counts in the header itself.
    #[test]
    fn a_running_card_with_progress_shows_its_tail_on_one_faint_row() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(with_progress(
            3 * anim::TICK_HZ,
            3 * anim::TICK_HZ,
            "Compiling hotl-engine",
            1204,
        ));
        let rows = draw(&s);
        assert!(
            rows[0].contains("Bash  cargo build · 3s · 1,204 lines"),
            "{:?}",
            rows[0]
        );
        assert!(rows[1].contains("Compiling hotl-engine"), "{:?}", rows[1]);
        let buf = draw_buffer(&s);
        let col = rows[1].find("Compiling").unwrap() as u16;
        assert_eq!(
            buf.cell((col, 1)).unwrap().style().fg,
            Some(Palette::default().faint)
        );
    }

    /// A long tail must never wrap: three rows one frame and one the next
    /// would make the whole transcript jump.
    #[test]
    fn the_tail_row_is_clipped_never_wrapped() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(with_progress(
            anim::TICK_HZ,
            anim::TICK_HZ,
            &"x".repeat(200),
            1,
        ));
        let rows = draw(&s);
        assert!(rows[0].contains("Bash"), "{:?}", rows[0]);
        assert!(rows[1].contains("xxx"), "{:?}", rows[1]);
        assert!(!rows[2].contains('x'), "the tail wrapped: {:?}", rows[2]);
    }

    /// The tail row takes the slot the result row will take, so the block's
    /// height does not jump at the moment the tool settles.
    #[test]
    fn a_settled_card_swaps_the_tail_row_for_the_result_row_at_the_same_height() {
        let mut s = State::new(true, "m".into());
        s.transcript
            .push(with_progress(anim::TICK_HZ, anim::TICK_HZ, "Compiling", 12));
        let running = draw(&s)[..STRIP]
            .iter()
            .filter(|r| !r.trim().is_empty())
            .count();

        let mut s = State::new(true, "m".into());
        let mut item = tool_item(
            "t1",
            "bash",
            "bash: cargo build",
            ToolStatus::Done,
            anim::TICK_HZ,
        );
        if let TranscriptItem::Tool { calls, .. } = &mut item {
            calls[0].lines = Some(12);
            calls[0].bytes = Some(480);
        }
        s.transcript.push(item);
        let settled = draw(&s)[..STRIP]
            .iter()
            .filter(|r| !r.trim().is_empty())
            .count();
        assert_eq!(running, settled, "the block changed height on settle");
    }

    /// Only `bash` has a sink, so only its silence means anything.
    #[test]
    fn a_silent_bash_card_says_quiet_after_ten_seconds() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(with_progress(
            22 * anim::TICK_HZ,
            10 * anim::TICK_HZ,
            "Compiling",
            12,
        ));
        let all = draw(&s)[..STRIP].join("\n");
        assert!(all.contains("quiet 12s"), "{all}");

        // Nine seconds of silence is not a stall.
        let mut s = State::new(true, "m".into());
        s.transcript.push(with_progress(
            19 * anim::TICK_HZ,
            10 * anim::TICK_HZ,
            "Compiling",
            12,
        ));
        assert!(
            !draw(&s)[..STRIP].join("\n").contains("quiet"),
            "{:?}",
            draw(&s)[0]
        );
    }

    #[test]
    fn a_silent_read_card_never_says_quiet() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(tool_item(
            "t1",
            "read",
            "read app.rs",
            ToolStatus::Running,
            30 * anim::TICK_HZ,
        ));
        let all = draw(&s)[..STRIP].join("\n");
        assert!(
            !all.contains("quiet"),
            "a tool with no sink said quiet: {all}"
        );
    }

    /// Everything the tail row renders must invalidate the cached rows.
    #[test]
    fn progress_enters_the_fingerprint() {
        let base = with_progress(anim::TICK_HZ, 0, "Compiling", 12);
        let none = tool_item(
            "t1",
            "bash",
            "bash: cargo build",
            ToolStatus::Running,
            anim::TICK_HZ,
        );
        assert_ne!(item_fingerprint(&base), item_fingerprint(&none));
        for mutate in [
            |pr: &mut crate::app::ToolProgress| pr.tail = "Linking".into(),
            |pr: &mut crate::app::ToolProgress| pr.lines = 13,
            |pr: &mut crate::app::ToolProgress| pr.at_ticks += 1,
        ] {
            let mut b = base.clone();
            if let TranscriptItem::Tool {
                progress: Some(pr), ..
            } = &mut b
            {
                mutate(pr);
            }
            assert_ne!(item_fingerprint(&base), item_fingerprint(&b));
        }
        // `bytes` is carried, never rendered.
        let mut b = base.clone();
        if let TranscriptItem::Tool {
            progress: Some(pr), ..
        } = &mut b
        {
            pr.bytes += 1_000;
        }
        assert_eq!(item_fingerprint(&base), item_fingerprint(&b));
    }

    /// 0061 T24: parked, not working — a hollow glyph in the quietest role,
    /// and the depth it is waiting behind.
    #[test]
    fn a_queued_card_wears_a_hollow_glyph_and_its_queue_depth() {
        let p = Palette::default();
        assert_eq!(
            status_glyph(&ToolStatus::Queued { ahead: 2 }, 0, &p),
            ("○", p.faint)
        );
        let mut s = State::new(true, "m".into());
        s.transcript.push(tool_item(
            "t1",
            "bash",
            "bash: cargo test",
            ToolStatus::Queued { ahead: 2 },
            0,
        ));
        let all = draw(&s)[..STRIP].join("\n");
        assert!(
            all.contains("○ Bash  cargo test · queued behind 2"),
            "{all}"
        );

        // Next in line reads as next, not as "behind 0".
        let mut s = State::new(true, "m".into());
        s.transcript.push(tool_item(
            "t1",
            "bash",
            "bash: cargo test",
            ToolStatus::Queued { ahead: 0 },
            0,
        ));
        let all = draw(&s)[..STRIP].join("\n");
        assert!(all.contains("queued · next"), "{all}");
    }

    /// Nothing has started, so there is no clock and no result.
    #[test]
    fn a_queued_card_has_no_elapsed_and_no_result_row() {
        let mut s = State::new(true, "m".into());
        let mut item = tool_item(
            "t1",
            "bash",
            "bash: cargo test",
            ToolStatus::Queued { ahead: 1 },
            9 * anim::TICK_HZ,
        );
        if let TranscriptItem::Tool { calls, .. } = &mut item {
            calls[0].lines = Some(3);
            calls[0].bytes = Some(9);
        }
        s.transcript.push(item);
        let all = draw(&s)[..STRIP].join("\n");
        assert!(!all.contains("9s"), "{all}");
        assert!(!all.contains('└'), "{all}");
    }

    /// The depth is rendered, so it must invalidate the cached rows.
    #[test]
    fn a_queue_depth_change_enters_the_fingerprint() {
        let a = tool_item(
            "t1",
            "bash",
            "bash: cargo test",
            ToolStatus::Queued { ahead: 1 },
            0,
        );
        let b = tool_item(
            "t1",
            "bash",
            "bash: cargo test",
            ToolStatus::Queued { ahead: 2 },
            0,
        );
        assert_ne!(item_fingerprint(&a), item_fingerprint(&b));
    }

    /// 0061 T3: a settled card's clock moves to a faint `└` row that also
    /// says how much the model got back.
    #[test]
    fn a_settled_card_with_counts_moves_elapsed_to_a_faint_result_row() {
        let mut s = State::new(true, "m".into());
        let mut item = tool_item(
            "t1",
            "bash",
            "cargo build",
            ToolStatus::Done,
            8 * anim::TICK_HZ,
        );
        if let TranscriptItem::Tool { calls, .. } = &mut item {
            calls[0].lines = Some(1204);
            calls[0].bytes = Some(51_233);
        }
        s.transcript.push(item);
        let rows = draw(&s);
        assert!(
            !rows[0].contains("8s"),
            "the header let go of the clock: {}",
            rows[0]
        );
        assert!(
            rows[1].trim_start().starts_with("└ 1,204 lines · 8s"),
            "result row: {:?}",
            rows[1]
        );
        let buf = draw_buffer(&s);
        let col = rows[1].find('└').unwrap() as u16;
        assert_eq!(
            buf.cell((col, 1)).unwrap().style().fg,
            Some(Palette::default().faint),
            "the result row is the quietest thing on the card"
        );
    }

    /// Zero bytes is a real answer, and it is not `0 lines`.
    #[test]
    fn a_silent_command_says_no_output() {
        let mut s = State::new(true, "m".into());
        let mut item = tool_item("t1", "bash", "true", ToolStatus::Done, anim::TICK_HZ);
        if let TranscriptItem::Tool { calls, .. } = &mut item {
            calls[0].lines = Some(0);
            calls[0].bytes = Some(0);
        }
        s.transcript.push(item);
        let all = draw(&s)[..STRIP].join("\n");
        assert!(all.contains("└ no output · 1s"), "{all}");
    }

    /// An older peer sends no counts: no row, and no elapsed anywhere — the
    /// card must not invent `0 lines` for a result it never heard about.
    #[test]
    fn a_card_without_counts_has_no_elapsed_and_no_row() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(tool_item(
            "t1",
            "read",
            "app.rs",
            ToolStatus::Done,
            3 * anim::TICK_HZ,
        ));
        // The input box draws its own `└`, so only the transcript counts.
        let all = draw(&s)[..STRIP].join("\n");
        assert!(!all.contains('└'), "no result row: {all}");
        assert!(!all.contains("3s"), "no elapsed: {all}");
    }

    /// A running card still owns its clock — the result row does not exist
    /// yet, so the header is the only place the elapsed can live.
    #[test]
    fn a_running_card_keeps_elapsed_in_its_header() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(tool_item(
            "t1",
            "bash",
            "cargo build",
            ToolStatus::Running,
            4 * anim::TICK_HZ,
        ));
        let rows = draw(&s);
        assert!(rows[0].contains("· 4s"), "{:?}", rows[0]);
        assert!(!rows[..STRIP].join("\n").contains('└'), "{:?}", rows);
    }

    /// Nothing ran, so there is nothing to count and no time to report.
    #[test]
    fn a_denied_card_has_no_result_row() {
        let mut s = State::new(true, "m".into());
        let mut item = tool_item(
            "t1",
            "write",
            "~/.ssh/config",
            ToolStatus::Denied,
            5 * anim::TICK_HZ,
        );
        if let TranscriptItem::Tool { calls, .. } = &mut item {
            calls[0].lines = Some(9);
            calls[0].bytes = Some(9);
        }
        s.transcript.push(item);
        let all = draw(&s)[..STRIP].join("\n");
        assert!(!all.contains('└'), "{all}");
        assert!(!all.contains("5s"), "{all}");
    }

    /// D2: work is a second voice. A card's body is muted and its glyph sits
    /// one column in from the prose column.
    #[test]
    fn a_card_body_is_muted_and_indented_one_column_past_prose() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::Assistant {
            text: "looking".into(),
        });
        s.transcript
            .push(tool_item("t1", "read", "app.rs", ToolStatus::Done, 0));
        let rows = draw(&s);
        let prose = rows.iter().find_map(|r| r.find('●')).expect("prose marker");
        let card = rows.iter().find_map(|r| r.find('→')).expect("card marker");
        assert_eq!(card, prose + 1, "cards sit one column in: {rows:?}");
    }

    /// One source of truth for the card indent: the width `item_visual_lines`
    /// reserves and the pad `Spine::wrap` writes must agree, or a card's rows
    /// wrap one column short of where they are drawn.
    #[test]
    fn every_items_indent_matches_its_spine() {
        let p = Palette::default();
        let items = [
            TranscriptItem::User { text: "hi".into() },
            TranscriptItem::Assistant { text: "hi".into() },
            TranscriptItem::Thinking { text: "mm".into() },
            TranscriptItem::Notice {
                text: "note".into(),
            },
            TranscriptItem::Error { text: "bad".into() },
            tool_item("t1", "read", "app.rs", ToolStatus::Done, 0),
            spawn_with_children(ToolStatus::Running, 1),
        ];
        for item in items {
            let (spine, _) = item_block(&item, &p, false, 40, true);
            assert_eq!(
                spine.indent,
                item_indent(&item),
                "indent disagrees for {item:?}"
            );
        }
    }

    #[test]
    fn a_single_call_is_singular() {
        let mut s = State::new(true, "m".into());
        s.transcript
            .push(spawn_with_children(ToolStatus::Running, 1));
        let all = draw(&s)[..STRIP].join("\n");
        assert!(all.contains("│ 1 call ·"), "{all}");
        assert!(!all.contains("1 calls"), "{all}");
    }

    // ---- 0049 T6: modals sized to content; code clips; scroll; cursor ----

    fn ask_with_diff(n_lines: usize) -> State {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "edit crates/gw/src/send.rs".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: (0..n_lines)
                .map(|i| crate::app::DiffLine {
                    op: DiffOp::Add,
                    text: format!(
                        "    let very_long_identifier_number_{i} = compute_something_with(base, timer, backoff, {i});"
                    ),
                })
                .collect(),
        };
        s
    }

    #[test]
    fn diff_rows_clip_instead_of_wrapping() {
        let rows = draw(&ask_with_diff(3));
        let clipped: Vec<&String> = rows
            .iter()
            .filter(|r| r.contains("very_long_identifier"))
            .collect();
        assert_eq!(clipped.len(), 3, "one row per diff line: {clipped:?}");
        assert!(clipped.iter().all(|r| r.contains('…')), "{clipped:?}");
    }

    #[test]
    fn a_modal_pads_one_cell_inside_its_border() {
        let rows = draw(&ask_with_diff(1));
        let row = rows.iter().find(|r| r.contains("edit crates")).unwrap();
        let i = row.find('│').unwrap();
        assert_eq!(&row[i + '│'.len_utf8()..][..1], " ", "{row:?}");
    }

    #[test]
    fn a_modal_widens_to_its_content_up_to_90_percent() {
        let rows = draw_at(&ask_with_diff(1), 120, 40);
        let top = rows.iter().find(|r| r.contains("waiting on you")).unwrap();
        let width = top.trim().chars().count();
        assert!(width > 72 && width <= 108, "60% < {width} <= 90%");
    }

    #[test]
    fn a_tall_ask_scrolls_with_pgdn() {
        let mut s = ask_with_diff(40);
        let first = draw(&s);
        assert!(
            first.iter().any(|r| r.contains("more · pgdn")),
            "{first:#?}"
        );
        assert!(first.iter().any(|r| r.contains("number_0 ")));
        s.modal_scroll = 10;
        let later = draw(&s);
        assert!(!later.iter().any(|r| r.contains("number_0 ")));
        assert!(later.iter().any(|r| r.contains("number_12")));
        // Past the end clamps: the last diff row and the key line show.
        s.modal_scroll = 1_000;
        let end = draw(&s);
        assert!(end.iter().any(|r| r.contains("number_39")), "{end:#?}");
        assert!(end.iter().any(|r| r.contains("y allow")), "{end:#?}");
        assert!(!end.iter().any(|r| r.contains("more · pgdn")));
    }

    #[test]
    fn the_question_modal_lists_options_with_descriptions_below() {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::WaitingQuestion {
            req_id: 1,
            header: "h".into(),
            prompt: "p".into(),
            input: String::new(),
            selected: 0,
            options: vec![
                hotl_tools::ask::QuestionOption {
                    label: "A".into(),
                    description: Some("first".into()),
                },
                hotl_tools::ask::QuestionOption {
                    label: "B".into(),
                    description: None,
                },
            ],
        };
        let rows = draw(&s);
        let a = rows.iter().position(|r| r.contains("› 1  A")).unwrap();
        assert!(
            rows[a + 1].contains("first"),
            "description on its own row: {:?}",
            rows[a + 1]
        );
        assert!(rows[a + 2].contains("  2  B"));
        assert!(
            rows.iter().any(|r| r.contains("↑↓ or 1-2 pick")),
            "{rows:#?}"
        );
    }

    #[test]
    fn a_wrapped_description_keeps_its_indent() {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::WaitingQuestion {
            req_id: 1,
            header: "h".into(),
            prompt: "p".into(),
            input: String::new(),
            selected: 0,
            options: vec![hotl_tools::ask::QuestionOption {
                label: "A".into(),
                description: Some("word ".repeat(30).trim().into()),
            }],
        };
        let rows = draw(&s);
        let a = rows.iter().position(|r| r.contains("› 1  A")).unwrap();
        let desc: Vec<&String> = rows[a + 1..]
            .iter()
            .take_while(|r| r.contains("word"))
            .collect();
        assert!(desc.len() >= 2, "wrapped: {desc:?}");
        for r in &desc {
            assert!(r.contains("│      word"), "hanging indent: {r:?}");
        }
    }

    /// At 60 columns every chip fits beside `bash · 3s · 2/4` exactly — the
    /// chips' own padding is the only gap the strip reserves.
    #[test]
    fn the_todo_count_survives_every_chip_at_60_cols() {
        let mut s = busy_strip_state();
        s.flag_count = 2;
        s.live_context = Some(s.context_window * 38 / 100);
        let row = strip_row(&draw_at(&s, 60, 18), "bypass");
        assert!(
            row.contains("bash · 3s · 2/4 ⚑ 2  38%  bypass  retry-dedupe"),
            "{row}"
        );
    }

    #[test]
    fn a_clipped_row_counts_cells_not_chars() {
        let line = Line::raw("日本語テキスト");
        assert_eq!(clip_line(&line, 20).to_string(), "日本語テキスト");
        assert_eq!(clip_line(&line, 7).to_string(), "日本語…");
    }

    // ---- 0049 T5: a measure for prose ----

    #[test]
    fn prose_wraps_at_the_measure_on_a_wide_terminal() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![TranscriptItem::Assistant {
            text: "word ".repeat(40).trim().into(), // 199 chars
        }];
        let rows = draw_at(&s, 200, 24);
        let prose: Vec<&String> = rows.iter().filter(|r| r.contains("word")).collect();
        assert_eq!(prose.len(), 2, "two rows at measure 110: {prose:?}");
        assert!(
            prose
                .iter()
                .all(|r| r.trim_end().chars().count() <= 4 + 110),
            "{prose:?}"
        );
    }

    #[test]
    fn cards_and_code_ignore_the_measure() {
        let mut s = State::new(true, "m".into());
        let path = format!("crates/{}.rs", "x".repeat(150));
        s.transcript = vec![
            tool_item("t1", "read", &path, ToolStatus::Done, 0),
            TranscriptItem::Assistant {
                text: format!("```\n{}\n```", "y".repeat(150)).into(),
            },
        ];
        let rows = draw_at(&s, 200, 24);
        assert!(rows.iter().any(|r| r.contains(&path)), "card on one row");
        assert!(
            rows.iter().any(|r| r.contains(&"y".repeat(150))),
            "code on one row"
        );
    }

    #[test]
    fn measure_zero_means_full_width_and_a_change_rewraps() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![TranscriptItem::Assistant {
            // 149 chars: over the measure, under the 196 the width leaves.
            text: "word ".repeat(30).trim().into(),
        }];
        let mut cache = TranscriptCache::default();
        let _ = draw_cached_at(&s, &mut cache, 200, 24);
        let n = cache.rewraps();
        s.measure = usize::MAX;
        let rows = draw_cached_at(&s, &mut cache, 200, 24);
        assert!(cache.rewraps() > n, "measure is in Geometry");
        assert_eq!(rows.iter().filter(|r| r.contains("word")).count(), 1);
    }

    // ---- 0049 T4: bottom anchoring ----

    #[test]
    fn a_short_session_touches_the_strip() {
        let rows = draw_raw(&state_with_prose(), 80, 24);
        assert!(
            rows[STRIP - 1].contains(PROSE),
            "newest row is just above the strip: {:?}",
            rows[STRIP - 1]
        );
        assert_eq!(rows[0].trim(), "", "padding is at the top");
    }

    #[test]
    fn the_first_overflow_moves_no_row_that_was_already_on_screen() {
        let mut s = State::new(true, "m".into());
        s.transcript = (0..19)
            .map(|i| TranscriptItem::Notice {
                text: format!("n{i}").into(),
            })
            .collect();
        let before = draw_raw(&s, 80, 24); // 19 harness rows, no blanks: exactly full
        s.transcript
            .push(TranscriptItem::Notice { text: "n19".into() });
        let after = draw_raw(&s, 80, 24);
        // Follow scrolled one row: every row that stayed is where the previous row was.
        assert_eq!(&after[..STRIP - 1], &before[1..STRIP]);
    }

    #[test]
    fn the_agent_stream_anchors_to_the_strip_too() {
        let mut s = State::new(true, "m".into());
        s.transcript
            .push(spawn_with_children(ToolStatus::Running, 2));
        s.selected_agent = Some("s1".into());
        let rows = draw_raw(&s, 80, 24);
        // The band (main + one spawn row) lifts the strip above row 19.
        let strip = rows.iter().position(|r| r.contains(&still())).unwrap();
        assert!(
            rows[strip - 1].contains("read c2.rs"),
            "{:?}",
            rows[strip - 1]
        );
        assert_eq!(rows[0].trim(), "");
    }

    // ---- 0049 T2: blank rows between speakers, not items ----

    #[test]
    fn comfortable_blanks_between_speakers_not_items() {
        let mut s = State::new(true, "m".into()); // Comfortable is the default
        s.transcript = vec![
            TranscriptItem::User { text: "hi".into() },
            tool_item("t1", "read", "a.rs", ToolStatus::Done, 0),
            tool_item("t2", "bash", "cargo test", ToolStatus::Failed, 0),
            TranscriptItem::Notice {
                text: "retrying".into(),
            },
            TranscriptItem::Assistant { text: "yo".into() },
        ];
        let rows = draw(&s);
        assert!(rows[0].starts_with("  ❯ hi"), "{:?}", rows[0]);
        assert_eq!(rows[1].trim(), "", "blank: you → harness");
        assert!(rows[2].contains("Read"), "{:?}", rows[2]);
        assert!(rows[3].contains("Bash"), "cards stack: {:?}", rows[3]);
        assert!(
            rows[4].contains("retrying"),
            "notice rides the run: {:?}",
            rows[4]
        );
        assert_eq!(rows[5].trim(), "", "blank: harness → model");
        assert!(rows[6].starts_with("  ● yo"), "{:?}", rows[6]);
    }

    #[test]
    fn a_prompt_always_gets_a_blank_before_it() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![
            TranscriptItem::Steer {
                text: "left".into(),
                queued: false,
            },
            TranscriptItem::User {
                text: "again".into(),
            },
        ];
        let rows = draw(&s);
        assert_eq!(rows[1].trim(), "", "same speaker, still a blank before ❯");
    }

    #[test]
    fn spacious_keeps_a_blank_between_every_item() {
        let mut s = State::new(true, "m".into());
        s.density = hotl_theme::Density::Spacious;
        s.transcript = vec![
            tool_item("t1", "read", "a.rs", ToolStatus::Done, 0),
            tool_item("t2", "read", "b.rs", ToolStatus::Done, 0),
        ];
        let rows = draw(&s);
        assert_eq!(rows[1].trim(), "");
        assert!(rows[2].contains("b.rs"));
    }

    /// The last resort when nothing droppable is left: the text is cut at a
    /// separator, never mid-token, and a lone overlong token is cut hard.
    /// 0061 T20: the first Esc is spent, so the row says what the second one
    /// does — in both editor modes.
    #[test]
    fn the_hint_names_the_second_esc_while_an_interrupt_is_in_flight() {
        for vim in [false, true] {
            let mut s = State::new(true, "m".into());
            s.vim_mode = vim;
            s.phase = Phase::Streaming { ticks: 0, chars: 0 };
            s.interrupt_sent = true;
            let rows = draw(&s);
            assert!(
                rows[HINT].contains("esc again takes control back · ctrl-c quits"),
                "vim={vim}: {}",
                rows[HINT]
            );
        }
    }

    /// 0061 T19: `fit_strip` hands back segments now, so the view can paint a
    /// blocked one loud — but the text they join to must be exactly what the
    /// single string used to be, at every width.
    #[test]
    fn fit_strip_segments_join_to_the_old_strings() {
        let segs = || {
            vec![
                anim::Segment::rank("bash · 3s", anim::KEEP),
                anim::Segment::rank("2/4", 1),
            ]
        };
        for width in [4usize, 8, 12, 20, 40] {
            let (kept, _) = fit_strip(segs(), Vec::new(), 0, width);
            let joined = anim::join(&kept);
            assert!(
                joined.chars().count() <= width.saturating_sub(1),
                "width {width}: {joined:?}"
            );
        }
        // Wide enough for everything: nothing is touched.
        let (kept, _) = fit_strip(segs(), Vec::new(), 0, 40);
        assert_eq!(anim::join(&kept), "bash · 3s · 2/4");
    }

    /// A blocked segment never folds and never drops, and keeps its tone
    /// through the fit — it is the one thing on the strip that must survive.
    #[test]
    fn a_blocked_segment_survives_the_fit_with_its_tone() {
        let segs = vec![
            anim::Segment::rank("bash · 3s", anim::KEEP),
            anim::Segment::blocked("quiet 12s"),
            anim::Segment::rank("test-model", 5),
        ];
        let (kept, _) = fit_strip(segs, Vec::new(), 0, 24);
        let blocked: Vec<_> = kept
            .iter()
            .filter(|s| s.tone == anim::Tone::Blocked)
            .collect();
        assert_eq!(blocked.len(), 1, "{kept:?}");
        assert_eq!(blocked[0].text, "quiet 12s");
        assert!(
            !kept.iter().any(|s| s.text == "test-model"),
            "the droppable segment went first: {kept:?}"
        );
    }

    #[test]
    fn elision_cuts_at_a_separator() {
        assert_eq!(elide_at_separator("bash · 3s · 2/4", 20), "bash · 3s · 2/4");
        assert_eq!(elide_at_separator("bash · 3s · 2/4", 12), "bash · 3s …");
        assert_eq!(elide_at_separator("bash · 3s · 2/4", 8), "bash …");
        assert_eq!(elide_at_separator("longtoken", 5), "long…");
        assert_eq!(elide_at_separator("x", 0), "");
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
            selected: 0,
        };
        let rows = draw(&s);
        let all = rows.join("\n");
        assert!(all.contains("Scope"), "header in modal");
        assert!(all.contains("How far?"), "prompt in modal");
        assert!(all.contains("› 1  MVP"), "numbered option: {all}");
        assert!(all.contains("  2  Full"), "second option: {all}");
        assert!(all.contains("everything"), "description shown: {all}");
        assert!(rows[STRIP].contains("waiting on you"), "halted strip");
    }

    /// 0047 P0 T6: a question a "yes" cannot authorize must not dress as a
    /// permission ask — blocked + "waiting on you" are exclusive to
    /// authority (design D3). The strip still says blocked, which is true.
    #[test]
    fn the_question_modal_wears_its_own_title_and_accent_border() {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::WaitingQuestion {
            req_id: 9,
            header: "Scope".into(),
            prompt: "How far?".into(),
            options: vec![hotl_tools::ask::QuestionOption {
                label: "MVP".into(),
                description: None,
            }],
            input: String::new(),
            selected: 0,
        };
        let buf = draw_buffer(&s);
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol())
                    .collect()
            })
            .collect();
        let all = rows.join("\n");
        assert!(all.contains("┌ a question for you"), "{all}");
        assert!(!all.contains("┌ waiting on you"), "{all}");
        assert!(
            rows[STRIP].contains("waiting on you"),
            "the strip still says blocked"
        );
        let y = rows
            .iter()
            .position(|r| r.contains("┌ a question for you"))
            .unwrap() as u16;
        let x = rows[y as usize].chars().position(|c| c == '┌').unwrap() as u16;
        let p = Palette::default();
        assert_eq!(
            buf.cell((x, y)).unwrap().style().fg,
            Some(p.accent),
            "accent border"
        );
        assert_ne!(
            p.accent, p.blocked,
            "the probe is only meaningful if the two differ"
        );
    }

    #[test]
    fn tool_card_and_strip_share_elapsed() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(tool_item(
            "t1",
            "bash",
            "echo hi",
            ToolStatus::Running,
            2 * anim::TICK_HZ,
        ));
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 2 * anim::TICK_HZ,
        };
        let rows = draw(&s);
        assert!(
            rows[STRIP].contains("bash · 2s"),
            "strip elapsed: {}",
            rows[STRIP]
        );
        assert!(
            rows.iter().any(|r| r.contains("Bash  echo hi · 2s")),
            "card elapsed"
        );
    }

    /// 0039: with no spawn cards the selector band is zero-height — every
    /// pre-existing row-indexed layout test stays honest.
    #[test]
    fn no_spawns_means_no_selector_row() {
        let mut s = State::new(true, "m".into());
        s.transcript
            .push(tool_item("t1", "read", "read x", ToolStatus::Done, 0));
        assert_eq!(selector_height(&s), 0);
        let rows = draw(&s);
        assert!(
            rows[HINT].contains("history"),
            "the hint keeps its pinned row: {}",
            rows[HINT]
        );
    }

    /// 0039/0043 D1: the band — `● main` plus one windowed row per *running*
    /// spawn, radio bullet on the shown stream, `… +N more` past the window.
    /// Settled cards neither render nor count toward the overflow.
    #[test]
    fn the_selector_lists_main_and_spawns_with_radio_bullets_and_overflow() {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        for i in 1..=6 {
            s.transcript.push(tool_item(
                &format!("s{i}"),
                "spawn",
                &format!("spawn task {i}"),
                ToolStatus::Running,
                0,
            ));
            if i % 3 == 0 {
                s.transcript.push(tool_item(
                    &format!("d{i}"),
                    "spawn",
                    &format!("spawn done {i}"),
                    ToolStatus::Done,
                    0,
                ));
            }
        }
        s.selected_agent = Some("s1".into());
        let all = draw(&s).join("\n");
        assert!(all.contains("○ main"), "main unselected: {all}");
        assert!(
            all.contains("● ⠑ spawn task 1 · 0s"),
            "selected spawn: {all}"
        );
        assert!(all.contains("○ ⠑ spawn task 2 · 0s"), "sibling row: {all}");
        assert!(
            !all.contains("spawn done"),
            "settled cards never list: {all}"
        );
        assert!(
            all.contains("… +2 more"),
            "overflow counts running only: {all}"
        );
    }

    /// 0043 D1: a band of settled cards is no band — zero height, default hint.
    #[test]
    fn all_settled_means_no_selector_row() {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        s.transcript
            .push(tool_item("s1", "spawn", "spawn a", ToolStatus::Done, 0));
        s.transcript
            .push(tool_item("s2", "spawn", "spawn b", ToolStatus::Failed, 0));
        assert_eq!(selector_height(&s), 0);
        let rows = draw(&s);
        // The running-turn hint, not the band's.
        assert!(rows[HINT].starts_with("esc interrupt"), "{}", rows[HINT]);
        assert!(!rows[HINT].contains("enter open"), "{}", rows[HINT]);
    }

    /// 0043 D3: the band cursor is a band-background highlight on its row —
    /// the `main` line included — while `●` keeps marking the shown stream.
    #[test]
    fn the_band_cursor_row_wears_the_band_background() {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        s.transcript.push(tool_item(
            "s1",
            "spawn",
            "spawn survey",
            ToolStatus::Running,
            0,
        ));
        s.band_cursor = Some(BandRow::Spawn("s1".into()));
        let band = Palette::default().band;
        let check = |s: &State, want_spawn: bool| {
            let buf = draw_buffer(s);
            let rows: Vec<String> = (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf.cell((x, y)).unwrap().symbol())
                        .collect()
                })
                .collect();
            let find = |needle: &str| rows.iter().position(|r| r.contains(needle)).unwrap() as u16;
            let has_band = |y: u16| {
                (0..buf.area.width).any(|x| buf.cell((x, y)).unwrap().style().bg == Some(band))
            };
            let (main_y, spawn_y) = (find("● main"), find("○ ⠑ spawn survey"));
            assert_eq!(has_band(spawn_y), want_spawn, "spawn row: {rows:#?}");
            assert_eq!(has_band(main_y), !want_spawn, "main row: {rows:#?}");
        };
        check(&s, true);
        s.band_cursor = Some(BandRow::Main);
        check(&s, false);
    }

    /// 0043: the 4-row window follows the cursor, not just the selection.
    #[test]
    fn the_band_window_follows_the_cursor() {
        let mut s = State::new(true, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        for i in 1..=6 {
            s.transcript.push(tool_item(
                &format!("s{i}"),
                "spawn",
                &format!("spawn task {i}"),
                ToolStatus::Running,
                0,
            ));
        }
        s.band_cursor = Some(BandRow::Spawn("s6".into()));
        let all = draw(&s).join("\n");
        assert!(
            all.contains("spawn task 6"),
            "the cursor row is in view: {all}"
        );
        assert!(
            !all.contains("spawn task 1 ·"),
            "the head scrolled off: {all}"
        );
        assert!(all.contains("… +2 more"), "{all}");
    }

    /// 0039 D7: selecting a spawn swaps the WHOLE region above the strip for
    /// its child stream — no floating modal — while strip, input, selector
    /// and hint render exactly as before.
    #[test]
    fn selecting_a_spawn_swaps_the_full_transcript_region_and_keeps_strip_input_hint() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::User {
            text: "main prose".into(),
        });
        s.transcript
            .push(spawn_with_children(ToolStatus::Running, 2));
        s.selected_agent = Some("s1".into());
        let rows = draw(&s);
        let all = rows.join("\n");
        assert!(
            all.contains("spawn survey · 0s · live · esc back"),
            "stream header: {all}"
        );
        assert!(
            all.contains("✓ read  read c1.rs"),
            "settled child row: {all}"
        );
        assert!(
            !all.contains("main prose"),
            "the transcript region swapped wholesale: {all}"
        );
        assert!(
            all.contains("● ⠑ spawn"),
            "the selector still renders: {all}"
        );
        assert!(
            rows[HINT].contains("esc back to main · pgup/pgdn scroll"),
            "stream hint: {}",
            rows[HINT]
        );
    }

    /// 0061 T29: a running child used to show no elapsed at all, which is
    /// exactly where a stuck one hides. It reads on the parent's live clock.
    #[test]
    fn a_running_child_shows_its_elapsed_on_the_parents_clock() {
        let mut s = State::new(true, "m".into());
        let mut item = spawn_with_children(ToolStatus::Running, 1);
        if let TranscriptItem::Tool {
            ticks, children, ..
        } = &mut item
        {
            *ticks = 30 + 2 * anim::TICK_HZ;
            children[0].started_at = 30;
            children[0].settled_at = None;
            children[0].ok = None;
        }
        s.transcript.push(item);
        s.selected_agent = Some("s1".into());
        let all = draw(&s).join("\n");
        assert!(all.contains("read c1.rs · 2s"), "{all}");
    }

    /// 0044: a settled child that reported a token total shows it after its
    /// duration, compactly; one that did not stays as before.
    #[test]
    fn the_agent_stream_shows_a_childs_token_total_when_known() {
        let mut s = State::new(true, "m".into());
        let mut item = spawn_with_children(ToolStatus::Running, 3);
        if let TranscriptItem::Tool { children, .. } = &mut item {
            children[0].tokens = Some(12_345);
        }
        s.transcript.push(item);
        s.selected_agent = Some("s1".into());
        let all = draw(&s).join("\n");
        assert!(
            all.contains("✓ read  read c1.rs · 0s · 12.3k tok"),
            "token total after the duration: {all}"
        );
        assert!(
            all.contains("read c2.rs · 0s") && !all.contains("read c2.rs · 0s ·"),
            "no total, no segment: {all}"
        );
    }

    /// 0039 D7: the stream follows its tail while `agent_scroll` is `None`
    /// and shows the top (header included) when scrolled back.
    #[test]
    fn the_agent_stream_follows_its_tail_and_scrolls_back() {
        let mut s = State::new(true, "m".into());
        s.transcript
            .push(spawn_with_children(ToolStatus::Running, 30));
        s.selected_agent = Some("s1".into());
        let all = draw(&s).join("\n");
        assert!(
            all.contains("read c30.rs"),
            "follow-tail shows the newest: {all}"
        );
        assert!(
            !all.contains("read c1.rs "),
            "the oldest scrolled off: {all}"
        );
        s.agent_scroll = Some(0);
        let all = draw(&s).join("\n");
        assert!(
            all.contains("spawn survey · 0s · live · esc back"),
            "the top shows the header: {all}"
        );
        assert!(all.contains("read c1.rs"), "…and the oldest child: {all}");
        assert!(
            !all.contains("read c30.rs"),
            "the newest is below the fold: {all}"
        );
    }

    /// 0043: the hint names the band keys while the cursor is engaged — the
    /// arrows without vim, j/k from vim Normal.
    #[test]
    fn the_hint_row_names_the_band_keys_while_engaged() {
        let mut s = State::new(false, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        s.transcript.push(tool_item(
            "s1",
            "spawn",
            "spawn survey",
            ToolStatus::Running,
            0,
        ));
        s.band_cursor = Some(BandRow::Main);
        let rows = draw(&s);
        assert!(
            rows[HINT].contains("↑↓ move · enter open · esc back"),
            "non-vim hint: {}",
            rows[HINT]
        );
        let mut s = State::new(true, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        s.transcript.push(tool_item(
            "s1",
            "spawn",
            "spawn survey",
            ToolStatus::Running,
            0,
        ));
        s.band_cursor = Some(BandRow::Main);
        s.editor
            .handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let rows = draw(&s);
        assert!(
            rows[HINT].contains("j/k move · enter open · esc back"),
            "vim Normal hint: {}",
            rows[HINT]
        );
    }

    // ---- 0047 P0 T4: help is generated, never hand-maintained ----

    /// The drift guard: `/goal` and `/workflows` were missing from the old
    /// hardcoded list.
    /// The help texts joined — what a reader can find on the overlay.
    fn help_text(state: &State) -> String {
        help_lines(state)
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn help_lists_every_builtin_command() {
        let all = help_text(&State::new(false, "m".into()));
        for cmd in crate::complete::builtins() {
            assert!(
                all.contains(&format!("/{}", cmd.name)),
                "/{} missing:\n{all}",
                cmd.name
            );
        }
    }

    #[test]
    fn help_hides_vim_keys_when_vim_is_off() {
        let off = help_lines(&State::new(false, "m".into()));
        assert!(off.iter().all(|(label, _)| *label != "vim"), "{off:#?}");
        assert!(!off.iter().any(|(_, l)| l.contains("j/k")), "{off:#?}");
        let on = help_lines(&State::new(true, "m".into()));
        assert!(on.iter().any(|(label, _)| *label == "vim"), "{on:#?}");
    }

    #[test]
    fn help_names_ctrl_g_not_ctrl_e_for_the_editor() {
        let all = help_text(&State::new(true, "m".into()));
        assert!(all.contains("ctrl-g $EDITOR"), "{all}");
        assert!(!all.contains("ctrl-e"), "{all}");
        assert!(all.contains("ctrl-a/e"), "{all}");
    }

    /// 0049 T7: every row of the table is on screen, whole, at 80×24 — vim
    /// on and off — and the labels sit in their own column.
    #[test]
    fn every_help_row_is_whole_at_80x24_in_both_modes() {
        for vim in [false, true] {
            let mut s = State::new(vim, "m".into());
            s.help_open = true;
            let rows = draw(&s);
            let all = rows.join("\n");
            for (_, text) in help_lines(&s) {
                assert!(all.contains(&text), "vim={vim}: clipped: {text}");
            }
            let composer = rows.iter().find(|r| r.contains("composer")).unwrap();
            assert!(composer.contains("composer   enter send"), "{composer:?}");
            // Sized from its widest row: past the 90% the other modals get,
            // never past 78 (the table's design width).
            let top = rows.iter().find(|r| r.contains("┌ keys")).unwrap();
            let width = top.trim().chars().count();
            assert!((73..=78).contains(&width), "{width}: {top:?}");
        }
    }

    #[test]
    fn help_scrolls_on_a_short_terminal() {
        let mut s = State::new(true, "m".into());
        s.help_open = true;
        let rows = draw_at(&s, 80, 14);
        assert!(rows.iter().any(|r| r.contains("more · pgdn")), "{rows:#?}");
        s.modal_scroll = 6;
        assert!(draw_at(&s, 80, 14)
            .iter()
            .any(|r| r.contains("any key closes")));
    }

    /// 0043: the help overlay names the agent band.
    #[test]
    fn the_help_overlay_names_the_agent_selector() {
        let mut s = State::new(true, "m".into());
        s.help_open = true;
        let all = draw(&s).join("\n");
        assert!(
            all.contains("agents     ↑↓ (j/k normal) band · enter open · esc back"),
            "{all}"
        );
    }

    /// 0039 D3: a card that absorbed N calls says so — `×N` rides the muted
    /// details, before the duration.
    #[test]
    fn a_merged_card_shows_a_multiplier_detail() {
        let mut s = State::new(true, "m".into());
        let mut item = tool_item("t1", "read", "read app.rs", ToolStatus::Done, anim::TICK_HZ);
        if let TranscriptItem::Tool { calls, .. } = &mut item {
            for id in ["t2", "t3", "t4"] {
                calls.push(crate::app::ToolCall {
                    id: (*id).into(),
                    ok: Some(true),
                    lines: None,
                    bytes: None,
                });
            }
        }
        s.transcript.push(item);
        let rows = draw(&s);
        assert!(
            rows.iter().any(|r| r.contains("Read  app.rs · ×4")),
            "the multiplier rides the settled header: {:?}",
            rows.first()
        );
    }

    fn spawn_with_children(status: ToolStatus, n: usize) -> TranscriptItem {
        let mut item = tool_item("s1", "spawn", "spawn survey", status, 0);
        if let TranscriptItem::Tool { children, .. } = &mut item {
            for i in 1..=n {
                children.push(crate::app::ChildCall {
                    id: format!("c{i}"),
                    name: "read".into(),
                    summary: format!("read c{i}.rs"),
                    // The last child still runs; earlier ones settled ok.
                    ok: (i < n).then_some(true),
                    started_at: 0,
                    settled_at: (i < n).then_some(0),
                    tokens: None,
                });
            }
        }
        item
    }

    // ---- 0061 T10: inline spans in prose ----

    /// The spans one prose line renders as `(text, fg, bold)`.
    fn spans_of(raw: &str) -> Vec<(String, Option<Color>, bool)> {
        let p = Palette::default();
        let mut fence = false;
        assistant_line(raw, &mut fence, &p)
            .0
            .spans
            .iter()
            .map(|s| {
                (
                    s.content.to_string(),
                    s.style.fg,
                    s.style.add_modifier.contains(Modifier::BOLD),
                )
            })
            .collect()
    }

    #[test]
    fn inline_code_spans_render_in_the_accent() {
        let p = Palette::default();
        assert_eq!(
            spans_of("run `cargo test` now"),
            vec![
                ("run ".to_string(), Some(p.ink), false),
                ("cargo test".to_string(), Some(p.accent), false),
                (" now".to_string(), Some(p.ink), false),
            ],
            "the backticks are stripped, not styled"
        );
    }

    /// Half a marker pair is a typo, not markup — eating it would lose
    /// characters the model wrote.
    #[test]
    fn an_unbalanced_backtick_stays_plain() {
        let p = Palette::default();
        assert_eq!(
            spans_of("half ` open"),
            vec![("half ` open".to_string(), Some(p.ink), false)]
        );
        assert_eq!(
            spans_of("a ** b"),
            vec![("a ** b".to_string(), Some(p.ink), false)]
        );
        // An empty pair is nothing to emphasize.
        assert_eq!(
            spans_of("a `` b"),
            vec![("a `` b".to_string(), Some(p.ink), false)]
        );
    }

    #[test]
    fn bold_spans_render_bold_in_the_strong_role() {
        let p = Palette::default();
        assert_eq!(
            spans_of("this **matters** a lot"),
            vec![
                ("this ".to_string(), Some(p.ink), false),
                ("matters".to_string(), Some(p.strong), true),
                (" a lot".to_string(), Some(p.ink), false),
            ]
        );
    }

    #[test]
    fn numbered_list_markers_render_in_the_accent() {
        let p = Palette::default();
        assert_eq!(
            spans_of("1. first"),
            vec![
                (String::new(), None, false),
                ("1. ".to_string(), Some(p.accent), false),
                ("first".to_string(), Some(p.ink), false),
            ]
        );
        // `12) ` counts too; `3.14` in prose does not.
        assert_eq!(spans_of("12) twelfth")[1].0, "12) ");
        assert_eq!(
            spans_of("3.14 is pi"),
            vec![("3.14 is pi".to_string(), Some(p.ink), false)]
        );
    }

    #[test]
    fn spans_compose_inside_bullets_and_numbered_items() {
        let p = Palette::default();
        let bullet = spans_of("- run `cargo test`");
        assert_eq!(bullet[1], ("• ".to_string(), Some(p.accent), false));
        assert_eq!(bullet[3], ("cargo test".to_string(), Some(p.accent), false));
        let numbered = spans_of("2. **do** it");
        assert_eq!(numbered[1], ("2. ".to_string(), Some(p.accent), false));
        assert_eq!(numbered[2], ("do".to_string(), Some(p.strong), true));
    }

    /// Headings and code keep their own treatment: a `#` line is bold ink and
    /// a fenced line is code, markers and all.
    #[test]
    fn headings_and_code_lines_keep_their_markers() {
        let p = Palette::default();
        assert_eq!(
            spans_of("# a `b` heading"),
            vec![("a `b` heading".to_string(), None, false)],
            "a heading keeps its backticks and stays one line-styled span"
        );
        let heading = assistant_line("# a `b` heading", &mut false, &p).0;
        assert_eq!(
            heading.style.fg,
            Some(p.ink),
            "heading colour is line-level"
        );
        let mut fence = true;
        let code = assistant_line("let x = `y`;", &mut fence, &p).0;
        assert_eq!(code.spans.len(), 1, "fenced code is untouched");
    }

    /// 0061 T9: the closing line is Harness voice at the prose column, in the
    /// quietest role — a full stop, not a result.
    #[test]
    fn the_turn_summary_is_one_faint_row_at_the_prose_column() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::Assistant {
            text: "done".into(),
        });
        s.transcript.push(TranscriptItem::TurnSummary {
            secs: 134,
            calls: 6,
            finished_at: Some("10:27".into()),
        });
        let rows = draw(&s);
        let at = rows
            .iter()
            .position(|r| r.contains('✻'))
            .expect("the summary row");
        assert!(
            rows[at].contains("✻ 2m 14s · 6 calls · done 10:27"),
            "{:?}",
            rows[at]
        );
        let prose = rows.iter().find_map(|r| r.find('●')).expect("prose marker");
        assert_eq!(
            rows[at].find('✻'),
            Some(prose),
            "the summary sits at the prose column, not the card's"
        );
        let buf = draw_buffer(&s);
        assert_eq!(
            buf.cell((prose as u16, at as u16)).unwrap().style().fg,
            Some(Palette::default().faint)
        );

        // No clock (an older peer, or Windows) simply omits it.
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::TurnSummary {
            secs: 3,
            calls: 0,
            finished_at: None,
        });
        let all = draw(&s)[..STRIP].join("\n");
        assert!(all.contains("✻ 3s"), "{all}");
        assert!(!all.contains("done"), "{all}");
        assert!(!all.contains("call"), "no calls, no count: {all}");
    }

    /// The bar is what makes the two rows read as one delegation rather than
    /// two unrelated cards.
    #[test]
    fn an_agent_card_is_a_two_row_block_with_a_bar() {
        let p = Palette::default();
        let item = spawn_with_children(ToolStatus::Running, 2);
        let (spine, rows) = item_block(&item, &p, false, 76, true);
        assert_eq!(spine.cont, "│");
        assert_eq!(spine.cont_style.fg, Some(p.faint));
        assert_eq!(spine.indent, 1);
        assert_eq!(rows.len(), 2, "title row plus the delegation row");
    }

    /// While it runs and nothing is drilled into, the block says how to open
    /// it. Drilled in, the keys belong to the stream, not the card.
    #[test]
    fn a_running_agent_card_names_the_band_keys() {
        let p = Palette::default();
        let running = spawn_with_children(ToolStatus::Running, 2);
        let text = |item: &TranscriptItem, band_keys| {
            item_block(item, &p, false, 76, band_keys).1[1]
                .0
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert!(text(&running, true).contains("↑↓ agents"));
        assert!(
            !text(&running, false).contains("↑↓ agents"),
            "a drill-in owns those keys"
        );
        let settled = spawn_with_children(ToolStatus::Done, 2);
        assert!(
            !text(&settled, true).contains("↑↓ agents"),
            "a finished agent is not somewhere to go"
        );
    }

    /// A spawn's `tool_done` carries counts like any other tool, but a line
    /// count of the child's report says nothing about the delegation — the
    /// second row is the block's own account of itself.
    #[test]
    fn an_agent_card_never_shows_a_line_count_row() {
        let mut s = State::new(true, "m".into());
        let mut item = spawn_with_children(ToolStatus::Done, 2);
        if let TranscriptItem::Tool { calls, .. } = &mut item {
            calls[0].lines = Some(1204);
            calls[0].bytes = Some(51_233);
        }
        s.transcript.push(item);
        let all = draw(&s)[..STRIP].join("\n");
        assert!(!all.contains("1,204 lines"), "{all}");
        assert!(!all.contains('└'), "{all}");
    }

    /// 0061 T8 supersedes 0042 D3 for agent cards alone: the brief keeps the
    /// title row and the delegation drops to a second row under a bar. Still
    /// no child rows and no `… +N earlier` churn.
    #[test]
    fn a_running_agent_card_puts_its_work_on_a_second_row() {
        let mut s = State::new(true, "m".into());
        s.transcript
            .push(spawn_with_children(ToolStatus::Running, 5));
        let rows = draw(&s);
        let all = rows[..STRIP].join("\n");
        assert!(all.contains("Agent  survey"), "title row: {all}");
        assert!(
            all.contains("│ 5 calls · 0s · Read c5.rs"),
            "count, elapsed then the newest call: {all}"
        );
        assert!(!all.contains("earlier"), "no tail block: {all}");
        assert!(
            !all.contains("read c4.rs"),
            "settled children stay behind the count: {all}"
        );
    }

    /// 0039: a settled spawn collapses its children to a `· N calls` detail —
    /// no child rows survive the settle.
    #[test]
    fn a_settled_spawn_card_collapses_children_to_a_call_count() {
        let mut s = State::new(true, "m".into());
        s.transcript.push(spawn_with_children(ToolStatus::Done, 5));
        let rows = draw(&s);
        let all = rows.join("\n");
        assert!(all.contains("│ 5 calls · 0s"), "collapsed count: {all}");
        assert!(!all.contains("read c5.rs"), "no child rows: {all}");
    }

    /// 0039 D7/T6: a child upsert re-wraps only the spawn card it lands on —
    /// the prose above it must not move.
    #[test]
    fn a_child_upsert_rewraps_only_the_spawn_card() {
        let mut s = cacheable_state();
        s.transcript.pop();
        s.transcript
            .push(spawn_with_children(ToolStatus::Running, 1));
        let mut cache = TranscriptCache::default();
        draw_cached(&s, &mut cache);
        assert_eq!(cache.rewraps(), 3, "three items, wrapped once each");
        if let Some(TranscriptItem::Tool { children, .. }) = s.transcript.last_mut() {
            children.push(crate::app::ChildCall {
                id: "c2".into(),
                name: "grep".into(),
                summary: "grep foo".into(),
                ok: None,
                started_at: 0,
                settled_at: None,
                tokens: None,
            });
        }
        draw_cached(&s, &mut cache);
        assert_eq!(cache.rewraps(), 4, "the push re-wraps only the card");
        if let Some(TranscriptItem::Tool { children, .. }) = s.transcript.last_mut() {
            children.last_mut().unwrap().ok = Some(true);
        }
        draw_cached(&s, &mut cache);
        assert_eq!(cache.rewraps(), 5, "the settle re-wraps only the card");
    }

    /// 0061 T5: the repeated `cd` prefix is the least informative thing on a
    /// bash card and the widest. Only `&&` elides; a `;` and a bare command
    /// pass through untouched.
    #[test]
    fn bash_body_elides_a_leading_cd_to_its_basename() {
        for (raw, want) in [
            (
                "cd /Users/x/sources/hotl && cargo test",
                "hotl ❯ cargo test",
            ),
            ("cd 'my repo' && ls", "my repo ❯ ls"),
            ("cd \"/tmp/a b\" && ls", "a b ❯ ls"),
            ("cd ../sibling && make", "sibling ❯ make"),
            ("cd .. && make", ".. ❯ make"),
            ("cd / && ls", "/ ❯ ls"),
            ("cd ~/src/hotl/ && ls", "hotl ❯ ls"),
            // Untouched: a `;` runs the tail regardless, and a bare command
            // has nothing to elide.
            ("cd /tmp; ls", "cd /tmp; ls"),
            ("cargo test", "cargo test"),
            ("cd /tmp &&", "cd /tmp &&"),
            ("cd 'unterminated && ls", "cd 'unterminated && ls"),
        ] {
            assert_eq!(bash_body(raw), want, "eliding {raw}");
        }
    }

    /// The elision is a card affordance. The ask renders the summary the
    /// engine wrote, verbatim — you approve what will run, not a shorthand.
    #[test]
    fn an_ask_keeps_the_verbatim_bash_summary() {
        let mut s = State::new(true, "m".into());
        let raw = "bash: cd /Users/x/sources/hotl && cargo test";
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: raw.into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        let all = draw_raw(&s, 100, 24).join("\n");
        assert!(
            all.contains("cd /Users/x/sources/hotl && cargo test"),
            "{all}"
        );
    }

    /// 0061 T4/T6: one table for every card verb and the phrase its rollup
    /// uses. Known tools get past tense where they leave something behind;
    /// anything else is title-cased per `_` word and takes the `N X calls`
    /// fallback. Agent cards never fold.
    #[test]
    fn tool_verb_table() {
        for (name, label, folded) in [
            ("bash", "Bash", Some("ran 4 shell commands")),
            ("read", "Read", Some("read 4 files")),
            ("write", "Wrote", Some("wrote 4 files")),
            ("edit", "Edited", Some("edited 4 files")),
            ("grep", "Searched", Some("searched 4 patterns")),
            ("glob", "Listed", Some("listed 4 patterns")),
            ("spawn", "Agent", None),
            ("workflow", "Workflow", None),
            ("todo_write", "Todo Write", Some("4 Todo Write calls")),
            ("skill", "Skill", Some("4 Skill calls")),
        ] {
            let verb = tool_verb(name);
            assert_eq!(verb.label, label, "verb for {name}");
            assert_eq!(verb.phrase(4).as_deref(), folded, "rollup for {name}");
        }
        // Singulars carry no `s`.
        assert_eq!(
            tool_verb("bash").phrase(1).as_deref(),
            Some("ran 1 shell command")
        );
        assert_eq!(tool_verb("read").phrase(1).as_deref(), Some("read 1 file"));
    }

    /// The card names the verb; the identity the reducer settles by is still
    /// the raw name, which `split_summary` keeps peeling.
    #[test]
    fn cards_wear_title_cased_verbs() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![
            tool_item("t1", "write", "write ./x", ToolStatus::Done, 0),
            tool_item("t2", "grep", "grep TODO", ToolStatus::Done, 0),
        ];
        let all = draw(&s)[..STRIP].join("\n");
        assert!(all.contains("Wrote  ./x"), "{all}");
        assert!(all.contains("Searched  TODO"), "{all}");
    }

    /// Success is quiet, failure is loud: the settled-ok glyph takes the muted
    /// role, a failure the blocked one.
    #[test]
    fn a_settled_ok_glyph_is_quiet_and_a_failure_is_loud() {
        let p = Palette::default();
        assert_eq!(status_glyph(&ToolStatus::Done, 0, &p), ("→", p.muted));
        assert_eq!(status_glyph(&ToolStatus::Failed, 0, &p), ("✗", p.blocked));
        assert_eq!(status_glyph(&ToolStatus::Denied, 0, &p), ("⊘", p.blocked));
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
        s.transcript.push(tool_item(
            "t1",
            "bash",
            "bash [sandboxed:seatbelt]: echo hi",
            ToolStatus::Done,
            anim::TICK_HZ,
        ));
        let rows = draw(&s);
        // Comfortable gutter (2) + the ✓ spine glyph; the name is no longer
        // bracketed, and the duplicate leading "bash" is peeled off the body.
        assert!(
            rows[0].starts_with("   → Bash  echo hi · sandboxed:seatbelt"),
            "spine card: {}",
            rows[0]
        );
        let buf = draw_buffer(&s);
        let p = Palette::default();
        let col = |needle: &str| rows[0][..rows[0].find(needle).unwrap()].chars().count() as u16;
        assert_eq!(
            buf.cell((col("echo"), 0)).unwrap().style().fg,
            Some(p.muted),
            "the whole card is a second voice under the prose"
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
        assert!(rows.contains("⤷ go left · queued"), "pinned chip");
        s.transcript[0] = TranscriptItem::Steer {
            text: "go left".into(),
            queued: false,
        };
        let rows = draw(&s).join("\n");
        assert!(rows.contains("⤷ go left"), "chip stays");
        assert!(!rows.contains("queued"), "queued tag gone once admitted");
    }

    #[test]
    fn strip_wears_band_and_running_tool_marker_is_active() {
        let mut s = State::new(true, "m".into());
        s.transcript
            .push(tool_item("t1", "bash", "echo hi", ToolStatus::Running, 0));
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
        // The resting wave still renders on the left.
        assert!(
            rows[STRIP].contains(&still()),
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
            .draw(|f| {
                view(
                    state,
                    &Palette::default(),
                    &mut TranscriptCache::default(),
                    f,
                )
            })
            .unwrap();
        let p = terminal.get_cursor_position().unwrap();
        (p.x, p.y)
    }

    /// The input box's rows, gutter, borders and the padding cell stripped.
    fn input_body(rows: &[String]) -> Vec<String> {
        rows.iter()
            .filter(|r| r.trim_start().starts_with('\u{2502}'))
            .map(|r| {
                r.trim_start()
                    .trim_matches('\u{2502}')
                    .strip_prefix(' ')
                    .unwrap_or_default()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn input_wraps_an_overlong_line_and_grows_the_box() {
        let mut s = State::new(true, "m".into());
        let long = "abcdefghij".repeat(12); // 120 chars into a 75-col box
        s.editor.set_text(&long);
        let rows = draw(&s);
        let body = input_body(&rows);
        assert_eq!(body.len(), 2, "box grew to two rows: {body:#?}");
        assert_eq!(body.concat(), long, "every typed char survives the wrap");
        // The cursor follows onto the second row instead of pinning to the edge.
        assert_eq!(
            draw_cursor(&s),
            (TEXT_COL + 45, 21),
            "cursor rides the wrap"
        );
    }

    #[test]
    fn input_renders_every_line_of_a_multiline_buffer() {
        let mut s = State::new(true, "m".into());
        s.editor.set_text("first line\nsecond line\nthird line");
        let body = input_body(&draw(&s));
        assert_eq!(body, ["first line", "second line", "third line"]);
        assert_eq!(
            draw_cursor(&s),
            (TEXT_COL + 10, 21),
            "cursor on the last line"
        );
    }

    #[test]
    fn a_buffer_taller_than_the_box_scrolls_to_the_cursor() {
        let mut s = State::new(true, "m".into());
        let text: Vec<String> = (0..20).map(|i| format!("line{i}")).collect();
        s.editor.set_text(&text.join("\n"));
        let rows = draw(&s);
        let body = input_body(&rows);
        assert_eq!(body.len(), 24 / 3, "box stops growing at a third");
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
        s.transcript.push(TranscriptItem::Assistant {
            text: text.clone().into(),
        });
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
        // [fence marker, code, code] — every one full-width.
        assert_eq!(lines[1].0.style.bg, Some(p.band));
        assert_eq!(lines[2].0.style.bg, Some(p.band));
        assert!(lines.iter().all(|(_, full)| *full));
    }

    #[test]
    fn follow_scroll_lands_on_the_last_line_with_spacing() {
        // Enough turns to overflow the 19-row transcript, so Follow has to
        // account for the blank separators too.
        let mut s = State::new(true, "m".into());
        for i in 0..30 {
            s.transcript.push(TranscriptItem::Assistant {
                text: format!("answer {i}").into(),
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
                text: format!("{i} {}", "x".repeat(200)).into(),
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
        assert_eq!(body.len(), 2, "75 columns holds 37 wide glyphs: {body:#?}");
        // A wide glyph owns two cells, the second rendered as a blank.
        assert_eq!(body[0].matches('\u{65e5}').count(), 37);
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
            rows[HINT].starts_with("esc interrupt") && !rows[HINT].contains("tab complete"),
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
        // left border: `│› /bare` and nothing after the name. The popup
        // sits on the gutter, with the box (0049 T3).
        assert_eq!(
            row.trim_end().trim_end_matches('│').trim_end(),
            "  │› /bare"
        );
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

    /// A session with a running tool card and more prose than fits on screen
    /// — long enough that `Scroll::At` and `Follow` land on different rows.
    fn cacheable_state() -> State {
        let mut s = State::new(true, "m".into());
        s.transcript.push(TranscriptItem::User {
            text: "explain the cache".into(),
        });
        s.transcript.push(TranscriptItem::Assistant {
            text: "a ".repeat(1200).into(),
        });
        s.transcript
            .push(tool_item("t1", "bash", "echo hi", ToolStatus::Running, 0));
        s
    }

    fn bump_tool_ticks(s: &mut State, to: u64) {
        let Some(TranscriptItem::Tool { ticks, .. }) = s.transcript.last_mut() else {
            unreachable!("the fixture ends with a tool card")
        };
        *ticks = to;
    }

    // ---- 0061 T6: closed runs of settled cards fold to one line ----

    fn settled(id: &str, name: &str, summary: &str, secs: u64) -> TranscriptItem {
        let mut item = tool_item(id, name, summary, ToolStatus::Done, secs * anim::TICK_HZ);
        if let TranscriptItem::Tool { calls, .. } = &mut item {
            calls[0].lines = Some(10);
            calls[0].bytes = Some(40);
        }
        item
    }

    /// The run reads as one quiet line naming what happened, not four cards
    /// competing with the prose around them.
    #[test]
    fn a_closed_run_of_settled_cards_folds_to_one_muted_line() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![
            settled("t1", "bash", "bash: echo a", 1),
            settled("t2", "bash", "bash: echo b", 2),
            settled("t3", "read", "read app.rs", 0),
            TranscriptItem::Assistant {
                text: "done".into(),
            },
        ];
        let all = draw(&s)[..STRIP].join("\n");
        assert!(
            all.contains("→ ran 2 shell commands, read 1 file · 3s"),
            "{all}"
        );
        assert!(!all.contains("echo a"), "the cards are gone: {all}");
        let buf = draw_buffer(&s);
        let rows = draw(&s);
        let r = rows.iter().position(|r| r.contains("→ ran")).unwrap() as u16;
        let col = rows[r as usize].find("ran").unwrap() as u16;
        assert_eq!(
            buf.cell((col, r)).unwrap().style().fg,
            Some(Palette::default().muted),
            "the rollup is muted"
        );
    }

    /// Work you are still watching never collapses under you: a run only
    /// folds once something follows it.
    #[test]
    fn a_run_at_the_transcript_tail_stays_open() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![
            settled("t1", "bash", "bash: echo a", 1),
            settled("t2", "bash", "bash: echo b", 1),
        ];
        assert_eq!(
            fold_plan(&s.transcript, false),
            vec![RunFold::Show, RunFold::Show]
        );
        let all = draw(&s)[..STRIP].join("\n");
        assert!(all.contains("echo a") && all.contains("echo b"), "{all}");
    }

    /// Folding one card saves no rows and loses its detail.
    #[test]
    fn a_single_settled_card_never_folds() {
        let transcript = vec![
            settled("t1", "bash", "bash: echo a", 1),
            TranscriptItem::Assistant { text: "ok".into() },
        ];
        assert_eq!(
            fold_plan(&transcript, false),
            vec![RunFold::Show, RunFold::Show]
        );
    }

    /// Anything a rollup could not honestly summarize breaks the run.
    #[test]
    fn running_failed_denied_and_agent_cards_break_a_run() {
        for breaker in [
            tool_item("b", "bash", "bash: slow", ToolStatus::Running, 0),
            tool_item("b", "bash", "bash: bad", ToolStatus::Failed, 0),
            tool_item("b", "write", "write ~/.ssh/config", ToolStatus::Denied, 0),
            tool_item(
                "b",
                "bash",
                "bash: waiting",
                ToolStatus::Queued { ahead: 1 },
                0,
            ),
            spawn_with_children(ToolStatus::Done, 1),
        ] {
            let transcript = vec![
                settled("t1", "bash", "bash: echo a", 1),
                breaker.clone(),
                settled("t2", "bash", "bash: echo b", 1),
                TranscriptItem::Assistant { text: "ok".into() },
            ];
            assert_eq!(
                fold_plan(&transcript, false),
                vec![RunFold::Show; 4],
                "no run of two survives {breaker:?}"
            );
        }
    }

    /// D3 merges several reads of one file into one card. The rollup counts
    /// files, so it says `read 1 file` rather than inventing four.
    #[test]
    fn a_merged_read_counts_files_not_pages() {
        let mut merged = settled("t1", "read", "read app.rs", 1);
        if let TranscriptItem::Tool { calls, .. } = &mut merged {
            for id in ["t2", "t3", "t4"] {
                calls.push(crate::app::ToolCall {
                    id: id.into(),
                    ok: Some(true),
                    lines: Some(10),
                    bytes: Some(40),
                });
            }
        }
        assert_eq!(rollup_text(&[merged]), "read 1 file · 1s");
        // A merged bash card really did run four commands.
        let mut bash = settled("t1", "bash", "bash: echo", 1);
        if let TranscriptItem::Tool { calls, .. } = &mut bash {
            for id in ["t2", "t3", "t4"] {
                calls.push(crate::app::ToolCall {
                    id: id.into(),
                    ok: Some(true),
                    lines: Some(10),
                    bytes: Some(40),
                });
            }
        }
        assert_eq!(rollup_text(&[bash]), "ran 4 shell commands · 1s");
    }

    /// The fold is row selection over rows already cached: toggling it must
    /// not re-wrap anything.
    #[test]
    fn folding_changes_no_cached_rows() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![
            settled("t1", "bash", "bash: echo a", 1),
            settled("t2", "bash", "bash: echo b", 1),
            TranscriptItem::Assistant { text: "ok".into() },
        ];
        let mut cache = TranscriptCache::default();
        draw_cached(&s, &mut cache);
        let after_first = cache.rewraps();
        assert_eq!(after_first, 3, "three items, wrapped once each");
        s.tools_expanded = true;
        draw_cached(&s, &mut cache);
        s.tools_expanded = false;
        draw_cached(&s, &mut cache);
        assert_eq!(
            cache.rewraps(),
            after_first,
            "unfolding and refolding re-wrapped something"
        );
    }

    #[test]
    fn rollup_text_grammar() {
        assert_eq!(
            rollup_text(&[settled("t1", "bash", "bash: echo a", 12)]),
            "ran 1 shell command · 12s"
        );
        assert_eq!(
            rollup_text(&[
                settled("t1", "grep", "grep TODO", 0),
                settled("t2", "skill", "skill brainstorm", 0),
            ]),
            "searched 1 pattern, 1 Skill call · 0s"
        );
        // First-appearance order, not alphabetical.
        assert_eq!(
            rollup_text(&[
                settled("t1", "read", "read a.rs", 0),
                settled("t2", "bash", "bash: echo", 0),
                settled("t3", "read", "read b.rs", 0),
            ]),
            "read 2 files, ran 1 shell command · 0s"
        );
    }

    #[test]
    fn fmt_elapsed_table() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(12), "12s");
        assert_eq!(fmt_elapsed(59), "59s");
        assert_eq!(fmt_elapsed(134), "2m 14s");
        assert_eq!(fmt_elapsed(3600), "1h 00m");
        assert_eq!(fmt_elapsed(3720), "1h 02m");
    }

    /// Follow still lands on the last row when rows above it disappeared.
    #[test]
    fn follow_scroll_lands_on_the_last_line_with_a_folded_run() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![TranscriptItem::User { text: "go".into() }];
        for i in 0..40 {
            s.transcript.push(settled(
                &format!("t{i}"),
                "bash",
                &format!("bash: echo {i}"),
                1,
            ));
        }
        s.transcript.push(TranscriptItem::Assistant {
            text: "the last word".into(),
        });
        let rows = draw(&s);
        assert!(
            rows[..STRIP].iter().any(|r| r.contains("the last word")),
            "follow lost the tail: {rows:?}"
        );
        assert!(
            rows[..STRIP].join("\n").contains("→ ran 40 shell commands"),
            "{rows:?}"
        );
    }

    /// The rollup names its own key, the way the collapsed Thinking block
    /// does — discoverable without opening help.
    #[test]
    fn ctrl_o_unfolds_every_rollup_without_rewrapping() {
        let mut s = State::new(true, "m".into());
        s.transcript = vec![
            settled("t1", "bash", "bash: echo a", 1),
            settled("t2", "bash", "bash: echo b", 1),
            TranscriptItem::Assistant { text: "ok".into() },
        ];
        let folded = draw(&s)[..STRIP].join("\n");
        assert!(
            folded.contains("· ctrl-o"),
            "the key rides the line: {folded}"
        );
        s.tools_expanded = true;
        let open = draw(&s)[..STRIP].join("\n");
        assert!(open.contains("echo a") && open.contains("echo b"), "{open}");
        assert!(!open.contains("→ ran"), "no header row survives: {open}");
    }

    /// P0's rule: no commit ships a false key hint, so the binding and its
    /// help row land together.
    #[test]
    fn help_names_ctrl_o_for_tool_cards() {
        let s = State::new(true, "m".into());
        let all = help_lines(&s)
            .iter()
            .map(|(_, t)| t.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("ctrl-o tool cards"), "{all}");
    }

    /// Spacious puts a blank above every item. A hidden item is not an item.
    #[test]
    fn hidden_items_add_no_blank_at_spacious() {
        let mut s = State::new(true, "m".into());
        s.density = Density::Spacious;
        s.transcript = vec![
            settled("t1", "bash", "bash: echo a", 1),
            settled("t2", "bash", "bash: echo b", 1),
            settled("t3", "bash", "bash: echo c", 1),
            TranscriptItem::Assistant { text: "ok".into() },
        ];
        let rows = draw(&s);
        let at = rows.iter().position(|r| r.contains("→ ran")).unwrap();
        let next = rows.iter().position(|r| r.contains("● ok")).unwrap();
        assert_eq!(
            next - at,
            2,
            "one rollup row plus one blank before the answer: {rows:?}"
        );
    }

    /// 0061 T2: unlike a child's tokens, a call's line count IS rendered —
    /// it must invalidate the cached rows when it lands.
    #[test]
    fn a_calls_line_count_enters_the_fingerprint() {
        let a = tool_item("t1", "bash", "cargo build", ToolStatus::Done, 0);
        let mut b = a.clone();
        if let TranscriptItem::Tool { calls, .. } = &mut b {
            calls[0].lines = Some(1204);
        }
        assert_ne!(item_fingerprint(&a), item_fingerprint(&b));
        let mut c = b.clone();
        if let TranscriptItem::Tool { calls, .. } = &mut c {
            calls[0].bytes = Some(51_233);
        }
        assert_ne!(item_fingerprint(&b), item_fingerprint(&c));
    }

    /// 0044: a child's token total is drill-in data, like its tick stamps —
    /// it must not re-wrap the spawn card when the done frame lands.
    #[test]
    fn a_childs_tokens_stay_outside_the_fingerprint() {
        let a = spawn_with_children(ToolStatus::Done, 2);
        let mut b = a.clone();
        if let TranscriptItem::Tool { children, .. } = &mut b {
            children[0].tokens = Some(4321);
        }
        assert_ne!(a, b, "the fixture differs only in tokens");
        assert_eq!(item_fingerprint(&a), item_fingerprint(&b));
    }

    #[test]
    fn a_static_transcript_never_rewraps_however_long_the_wave_runs() {
        // The thinking phase: nothing in the transcript moves, only the strip.
        // Whatever TICK_HZ is, this must stay at the cost of the first frame.
        let mut s = cacheable_state();
        s.transcript.pop(); // drop the running card — nothing left that changes
        let mut cache = TranscriptCache::default();
        let first = draw_cached(&s, &mut cache);
        let after_first = cache.rewraps();
        assert_eq!(after_first, 2, "the first draw wraps every item once");

        for _ in 0..(5 * anim::TICK_HZ) {
            assert_eq!(draw_cached(&s, &mut cache), first, "rows drifted");
        }
        assert_eq!(
            cache.rewraps(),
            after_first,
            "five seconds of animation must not re-wrap a static transcript"
        );
    }

    #[test]
    fn a_running_turn_rewraps_only_the_item_that_moved() {
        let mut s = cacheable_state();
        let mut cache = TranscriptCache::default();
        draw_cached(&s, &mut cache);
        assert_eq!(cache.rewraps(), 3, "three items, wrapped once each");

        // A second of a running tool. The card's spinner and elapsed do
        // change, so *it* re-wraps — but the prose above it never does. Bound:
        // the spinner's own rate plus the one-second boundary, nowhere near
        // the 3 × TICK_HZ a whole-transcript cache would have cost.
        for t in 1..=anim::TICK_HZ {
            bump_tool_ticks(&mut s, t);
            draw_cached(&s, &mut cache);
        }
        let card_rewraps = cache.rewraps() - 3;
        assert!(
            card_rewraps <= MARKER_HZ + 1,
            "a second of tool spin re-wrapped {card_rewraps} times, want <= {}",
            MARKER_HZ + 1
        );

        // Streaming text: only the assistant item is touched per delta.
        let before = cache.rewraps();
        for _ in 0..10 {
            if let Some(TranscriptItem::Assistant { text }) = s.transcript.get_mut(1) {
                text.push_str(" delta");
            }
            draw_cached(&s, &mut cache);
        }
        assert_eq!(
            cache.rewraps() - before,
            10,
            "one re-wrap per delta — the user turn and the tool card must not move"
        );
    }

    #[test]
    fn running_marker_walks_the_watch_wanderer() {
        // Twin of watch-tui's WORKING_FRAMES; a drifted copy shows two
        // different organisms for the same work.
        assert_eq!(WORKING_FRAMES.len(), 16);
        assert_eq!(WORKING_FRAMES[0], "⠑");
        assert_eq!(WORKING_FRAMES[8], "⠔");
        // 8 fps over 16 frames: one full wander every 2 seconds, every frame
        // shown exactly once.
        let walked: Vec<usize> = (0..2 * anim::TICK_HZ).map(marker_frame).collect();
        assert_eq!(walked.first(), Some(&0));
        assert_eq!(walked.last(), Some(&15));
        let mut distinct = walked;
        distinct.dedup();
        assert_eq!(distinct.len(), 16);
    }

    #[test]
    fn scrolling_reuses_the_rows_it_already_has() {
        let mut s = cacheable_state();
        let mut cache = TranscriptCache::default();
        let follow = draw_cached(&s, &mut cache);
        let settled = cache.rewraps();

        s.scroll = Scroll::At(0);
        let top = draw_cached(&s, &mut cache);
        assert_eq!(cache.rewraps(), settled, "scrolling re-wrapped");
        assert_ne!(top, follow, "…but it did move the window");

        s.scroll = Scroll::Follow;
        assert_eq!(
            draw_cached(&s, &mut cache),
            follow,
            "scrolling back differs"
        );
        assert_eq!(cache.rewraps(), settled, "scrolling back re-wrapped");
    }

    #[test]
    fn geometry_and_theme_changes_drop_the_whole_memo() {
        let mut s = cacheable_state();
        let mut cache = TranscriptCache::default();
        let at = |s: &State, w: u16, cache: &mut TranscriptCache| {
            let mut terminal = Terminal::new(TestBackend::new(w, 24)).unwrap();
            terminal
                .draw(|f| view(s, &Palette::default(), cache, f))
                .unwrap();
        };
        at(&s, 80, &mut cache);
        at(&s, 80, &mut cache);
        assert_eq!(cache.rewraps(), 3, "same width must reuse every row");

        // Wrap width changed: every item's rows are stale at once.
        at(&s, 60, &mut cache);
        assert_eq!(cache.rewraps(), 6, "a resize must re-wrap all three");

        // So are the two other things every item's rows are a function of.
        s.thinking_expanded = !s.thinking_expanded;
        at(&s, 60, &mut cache);
        assert_eq!(cache.rewraps(), 9, "ctrl-t must re-wrap all three");
        s.density = hotl_theme::Density::Compact;
        at(&s, 60, &mut cache);
        assert_eq!(cache.rewraps(), 12, "density must re-wrap all three");
    }

    #[test]
    fn cached_rows_are_identical_to_a_fresh_render() {
        // The cache is only ever correct if a reused one and a cold one agree.
        // Walks the same mutations a real turn makes, comparing every frame.
        // Since 0039 the walk also absorbs/settles `calls` and pushes/settles
        // `children` — the ONLY enforcement of the fingerprint invariant for
        // the new fields.
        let mut s = cacheable_state();
        let mut warm = TranscriptCache::default();
        for step in 0..40u64 {
            // 0061 T25: the multiplier crosses `QUIET_AFTER` before the walk
            // ends, so the card's `quiet Ns` is exercised too.
            bump_tool_ticks(&mut s, step * 12);
            if step % 5 == 0 {
                if let Some(TranscriptItem::Assistant { text }) = s.transcript.get_mut(1) {
                    text.push_str(" delta");
                }
            }
            if step == 20 {
                s.scroll = Scroll::At(1);
            }
            // 0061 T24: a queue→promote step, so the walk covers the parked
            // card and the promotion that replaces it. Inserted *before* the
            // running card, which every mutation below still addresses as
            // `last_mut`.
            if step == 5 {
                let at = s.transcript.len() - 1;
                s.transcript.insert(
                    at,
                    tool_item(
                        "q1",
                        "bash",
                        "bash: cargo test",
                        ToolStatus::Queued { ahead: 1 },
                        0,
                    ),
                );
            }
            if step == 7 {
                if let Some(TranscriptItem::Tool { status, .. }) = s
                    .transcript
                    .iter_mut()
                    .find(|i| matches!(i, TranscriptItem::Tool { id, .. } if id == "q1"))
                {
                    *status = ToolStatus::Running;
                }
            }
            // 0061 T25: a tail lands once and then goes silent, so the walk
            // crosses `quiet Ns` with the cache warm.
            if step == 12 {
                if let Some(TranscriptItem::Tool {
                    ticks, progress, ..
                }) = s.transcript.last_mut()
                {
                    *progress = Some(crate::app::ToolProgress {
                        tail: "Compiling hotl-engine".into(),
                        lines: 1204,
                        bytes: 48_000,
                        at_ticks: *ticks,
                    });
                }
            }
            if let Some(TranscriptItem::Tool {
                calls, children, ..
            }) = s.transcript.last_mut()
            {
                match step {
                    10 => calls.push(crate::app::ToolCall {
                        id: "t2".into(),
                        ok: None,
                        lines: None,
                        bytes: None,
                    }),
                    15 => {
                        let c = calls.last_mut().unwrap();
                        c.ok = Some(true);
                        c.lines = Some(1204);
                        c.bytes = Some(51_233);
                    }
                    25 => children.push(crate::app::ChildCall {
                        id: "c1".into(),
                        name: "read".into(),
                        summary: "read a.rs".into(),
                        ok: None,
                        started_at: step,
                        settled_at: None,
                        tokens: None,
                    }),
                    30 => {
                        let c = children.last_mut().unwrap();
                        c.ok = Some(false);
                        c.settled_at = Some(step);
                    }
                    _ => {}
                }
            }
            let cold = draw_cached(&s, &mut TranscriptCache::default());
            let hot = draw_cached(&s, &mut warm);
            assert_eq!(hot, cold, "cached render diverged at step {step}");
        }
    }

    /// 0033 Task 3: after every append, at every chunk size, the cached
    /// incremental rows must equal a cold `item_visual_lines` of the full
    /// text — same split, classifier, wrap, and spine-first rule.
    #[test]
    fn incremental_assistant_rows_equal_cold_render() {
        // 0061 T10 extends the corpus with the inline forms: a code span, a
        // bold run, an unbalanced marker and a numbered item.
        let corpus = "# h\ntext **b**\n```rust\nlet x = 1;\n```\n- a `x`\n- **b**\n    code\nplain `y` and **z**\nhalf ` open\n1. first `f`\n2) second\n";
        let p = Palette::default();
        for chunk in 1..=9usize {
            let mut item = TranscriptItem::Assistant { text: "".into() };
            let mut rows: Vec<Line<'static>> = Vec::new();
            let mut inc = Incremental::new(match &item {
                TranscriptItem::Assistant { text } => text.seed(),
                _ => unreachable!(),
            });
            let mut fed = 0;
            while fed < corpus.len() {
                let mut end = (fed + chunk).min(corpus.len());
                while !corpus.is_char_boundary(end) {
                    end += 1;
                }
                let TranscriptItem::Assistant { text } = &mut item else {
                    unreachable!()
                };
                text.push_str(&corpus[fed..end]);
                fed = end;
                assistant_append(&mut rows, &mut inc, text.as_str(), &p, 40, 2, 30);
                let cold = item_visual_lines(&item, &p, 40, 2, false, 30, true);
                assert_eq!(rows, cold, "diverged at chunk={chunk} fed={fed}");
            }
        }
    }

    /// The cost side of the same change: streaming N chunks over an L-line
    /// answer classifies O(L + N) lines, not O(L × N).
    #[test]
    fn streaming_classifies_only_what_grew() {
        let corpus =
            "# h\ntext **b**\n```rust\nlet x = 1;\n```\n- a\n- b\n    code\nplain\n".repeat(30);
        let mut s = cacheable_state();
        s.transcript.pop();
        let mut cache = TranscriptCache::default();
        draw_cached(&s, &mut cache);
        let base = cache.line_wraps();
        let mut fed = 0;
        let mut appends = 0u64;
        while fed < corpus.len() {
            let end = (fed + 5).min(corpus.len());
            if let Some(TranscriptItem::Assistant { text }) = s.transcript.get_mut(1) {
                text.push_str(&corpus[fed..end]);
            }
            fed = end;
            appends += 1;
            draw_cached(&s, &mut cache);
        }
        let lines = corpus.lines().count() as u64;
        let spent = cache.line_wraps() - base;
        // Each append re-does at most the partial line plus what completed;
        // the whole stream costs every line once plus one partial per append.
        assert!(
            spent <= lines + 2 * appends + 8,
            "classified {spent} lines for {lines} lines in {appends} appends — not incremental"
        );
    }

    /// 0033 Task 3: the streaming shape specifically — hundreds of small
    /// deltas cutting lines, fences, headings and bullets at every offset;
    /// warm must equal cold after every single append.
    #[test]
    fn streamed_deltas_render_identically_to_a_fresh_render() {
        let corpus = "# head\ntext **b** and prose that wraps past the narrow test terminal \
                      width\n```rust\nlet x = 1;\n```\n- a\n- b\n    code\nplain tail\n"
            .repeat(4);
        let mut s = cacheable_state();
        s.transcript.pop(); // drop the running tool card; this is about text
        let mut warm = TranscriptCache::default();
        let mut fed = 0;
        let chunks = (1..=7).cycle();
        for (step, take) in chunks.enumerate() {
            if fed >= corpus.len() {
                break;
            }
            let mut end = (fed + take).min(corpus.len());
            while !corpus.is_char_boundary(end) {
                end += 1;
            }
            if let Some(TranscriptItem::Assistant { text }) = s.transcript.get_mut(1) {
                text.push_str(&corpus[fed..end]);
            }
            fed = end;
            let cold = draw_cached(&s, &mut TranscriptCache::default());
            let hot = draw_cached(&s, &mut warm);
            assert_eq!(hot, cold, "streamed render diverged at step {step}");
        }
    }

    /// A failed turn must be unmistakable: a ✗ and the blocked (error) color on
    /// both spine and body, so it never reads as a muted notice.
    #[test]
    fn an_error_item_renders_red_with_a_cross() {
        let p = Palette::default();
        let (spine, lines) = item_block(
            &TranscriptItem::Error {
                text: "HTTP 400: invalid_request_error: boom".into(),
            },
            &p,
            false,
            76,
            true,
        );
        assert_eq!(spine.marker, "✗");
        assert_eq!(spine.marker_style.fg, Some(p.blocked));
        let shown: String = lines
            .iter()
            .flat_map(|(l, _)| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            shown.contains("HTTP 400"),
            "the message must be shown: {shown}"
        );
        // `Line::styled` carries the color on the line, not its spans.
        assert!(
            lines.iter().all(|(l, _)| l.style.fg == Some(p.blocked)),
            "the error body is the blocked color, not muted: {lines:?}"
        );
    }

    // --- /context render (plan 0028) -----------------------------------

    /// The content width an 80-column terminal hands a transcript item at the
    /// default density: `80 - gutter(2) - glyph - space`.
    const REPORT_INNER: usize = 76;

    fn report(window: u64, reported: Option<u64>, rows: Vec<(ContextKind, u64)>) -> ContextReport {
        let estimated: u64 = rows.iter().map(|(_, n)| n).sum();
        ContextReport {
            model: "claude-opus-5".into(),
            window,
            reported,
            estimated,
            free: window.saturating_sub(estimated.max(reported.unwrap_or(0))),
            rows,
        }
    }

    fn block(r: &ContextReport, inner: usize) -> Vec<Line<'static>> {
        item_block(
            &TranscriptItem::Report(r.clone()),
            &Palette::default(),
            false,
            inner,
            true,
        )
        .1
        .into_iter()
        .map(|(l, _)| l)
        .collect()
    }

    /// Every foreground a one-character `glyph` span was drawn in, in order.
    fn glyph_colors(lines: &[Line], glyph: &str) -> Vec<Color> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.content == glyph)
            .map(|s| s.style.fg.expect("a group glyph always carries a color"))
            .collect()
    }

    fn three_groups() -> ContextReport {
        report(
            1_000_000,
            Some(241_300),
            vec![
                (ContextKind::SystemPrompt, 5_312),
                (ContextKind::ToolSchemas, 14_401),
                (ContextKind::Memory, 1_800),
                (ContextKind::Messages, 102_438),
                (ContextKind::ToolResults, 138_800),
            ],
        )
    }

    #[test]
    fn a_context_report_renders_one_line_per_row() {
        let r = three_groups();
        let lines = block(&r, REPORT_INNER);
        // header, blank, meter, blank, reported, estimated, blank, then the
        // five rows plus free space.
        assert_eq!(lines.len(), 7 + r.rows.len() + 1);
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text[0].contains("claude-opus-5") && text[0].contains("1.0M window"));
        assert!(text[4].starts_with("  reported"), "{:?}", text[4]);
        assert!(text[5].starts_with("  estimated"), "{:?}", text[5]);
        assert!(text.last().expect("free row").contains("free space"));
    }

    #[test]
    fn the_reported_line_is_absent_before_the_first_turn() {
        let r = report(200_000, None, vec![(ContextKind::Messages, 10_000)]);
        let text: Vec<String> = block(&r, REPORT_INNER)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(
            !text.iter().any(|l| l.contains("reported")),
            "no turn has reported anything yet: {text:?}"
        );
        assert!(text.iter().any(|l| l.contains("estimated")));
    }

    #[test]
    fn group_colors_differ_across_groups() {
        let lines = block(&three_groups(), REPORT_INNER);
        let prefix = glyph_colors(&lines, "▣")[0];
        let preamble = glyph_colors(&lines, "◆")[0];
        let conversation = glyph_colors(&lines, "▪")[0];
        assert_ne!(prefix, preamble);
        assert_ne!(preamble, conversation);
        assert_ne!(prefix, conversation);
    }

    #[test]
    fn rows_within_a_group_are_distinguishable() {
        let lines = block(&three_groups(), REPORT_INNER);
        for glyph in ["▣", "▪"] {
            let colors = glyph_colors(&lines, glyph);
            assert_eq!(colors.len(), 2, "{glyph}");
            assert_ne!(
                colors[0], colors[1],
                "two {glyph} rows must not share a color"
            );
        }
    }

    #[test]
    fn free_space_turns_blocked_when_the_window_is_nearly_full() {
        let p = Palette::default();
        let full = report(200_000, None, vec![(ContextKind::Messages, 180_000)]);
        assert_eq!(
            glyph_colors(&block(&full, REPORT_INNER), "▫"),
            vec![p.blocked]
        );
        let roomy = report(200_000, None, vec![(ContextKind::Messages, 20_000)]);
        assert_eq!(
            glyph_colors(&block(&roomy, REPORT_INNER), "▫"),
            vec![p.faint]
        );
    }

    #[test]
    fn the_meter_is_dropped_on_a_narrow_terminal() {
        let text: Vec<String> = block(&three_groups(), 20)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(
            !text.iter().any(|l| l.contains('▇')),
            "a two-cell bar lies more than no bar: {text:?}"
        );
        // The table itself still renders.
        assert!(text.iter().any(|l| l.contains("free space")));
    }

    #[test]
    fn meter_segments_sum_to_the_bar_width() {
        // A 0.4% row beside a 40% one is exactly the mix largest-remainder
        // exists for.
        let mixes = vec![
            three_groups(),
            report(
                200_000,
                None,
                vec![
                    (ContextKind::Todos, 800),
                    (ContextKind::Messages, 80_000),
                    (ContextKind::ToolResults, 1),
                ],
            ),
            report(200_000, Some(199_999), vec![(ContextKind::Messages, 1_000)]),
        ];
        for inner in [MIN_METER_COLS, 40, REPORT_INNER] {
            for r in &mixes {
                let bar = &block(r, inner)[2];
                let cells: usize = bar
                    .spans
                    .iter()
                    .skip(1) // the two-space indent
                    .map(|s| s.content.chars().count())
                    .sum();
                assert_eq!(cells, inner - 2, "inner={inner} rows={:?}", r.rows);
            }
        }
    }

    #[test]
    fn a_zero_window_renders_without_panicking() {
        let r = report(0, Some(0), vec![(ContextKind::Messages, 10)]);
        let text: Vec<String> = block(&r, REPORT_INNER)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(text.iter().any(|l| l.contains("messages")), "{text:?}");
    }
}
