//! Activity animation: a braille snake travelling a wave, gradient-lit from
//! the theme.
//!
//! Pure arithmetic — ticks arrive at `TICK_HZ` only while a turn runs, and
//! every frame is a function of the tick count alone (no wall-clock, no RNG,
//! no floats, so tests pin exact strings on every platform).
//!
//! A braille cell is 2 dots wide and 4 tall, so `WIDTH` cells give `2 * WIDTH`
//! sub-columns to draw in. The path is a fixed triangular wave across those
//! sub-columns; the snake is a finite body that travels along it, head first,
//! thinning to a single dot at the tail. The path itself never moves — a snake
//! swimming a still wave reads as one creature, where scrolling both reads as
//! noise. Phase is carried by *color*, not by shape: one motion language
//! across the whole strip, with a per-phase gradient whose two endpoints are
//! theme slots, so a preset or a single-slot override recolors the animation
//! with it. Salience is visual only — no bell, ever.

use crate::app::{Phase, State};
use hotl_theme::Palette;
use ratatui::style::Color;

/// Animation ticks per second. The runtime's ticker interval is derived from
/// this (`hotl::tui`), as is every tick→seconds display, so the rate is
/// changed here and nowhere else.
pub const TICK_HZ: u64 = 30;

/// Braille cells the animation occupies. Wide enough to read as motion, short
/// enough to leave the strip's one line for the text that follows it.
pub const WIDTH: usize = 12;

/// Drawable sub-columns: a braille cell is two dots wide.
const COLUMNS: usize = WIDTH * 2;

/// The snake's length in sub-columns. Long enough to show the wave it is
/// travelling and most of the phase gradient, short enough to keep a visible
/// head and tail rather than ringing the whole strip.
const BODY: usize = 14;

/// Sub-columns per wave crest. Long against `BODY` on purpose: the snake
/// undulates gently instead of buzzing.
const WAVELENGTH: usize = 16;

// The activity cycle, driven by the whole-turn clock (`State::work_ticks`), in
// order: swim one way for `TRAVEL`; rear the head up in the middle and glance
// both ways (`RISE` up, two `LOOK` holds, `FALL` back); swim back the other way
// for `TRAVEL`; glance again with the look order inverted. Durations are cut
// from `TICK_HZ`, not hard-coded frame counts, so they track the tick rate.

/// Ticks spent swimming across the strip in one direction (10s).
const TRAVEL: u64 = 10 * TICK_HZ;
/// Ticks the head holds a single glance (1s), once each way.
const LOOK: u64 = TICK_HZ;
/// Ticks for the head to spring up out of the middle (~0.33s).
const RISE: u64 = 10;
/// Ticks for the head to drop back down and resume swimming (~0.27s).
const FALL: u64 = 8;
/// One head-up interlude: rise, two glances, fall.
const INTERLUDE: u64 = RISE + 2 * LOOK + FALL;
/// The full loop: travel, glance, travel the other way, glance inverted.
const CYCLE: u64 = 2 * (TRAVEL + INTERLUDE);

/// A frame under construction: dot on/off at `[row][sub-column]`, `row` 0 (top)
/// to 3 (bottom), sub-column 0 (left) to `COLUMNS-1` (right). Built here, then
/// packed into braille cells by `cells_of`. Working in dots rather than cells
/// keeps the pose helpers readable and makes `mirror` a one-liner.
type Grid = [[bool; COLUMNS]; 4];

/// Braille dot bits by row, for the left and right sub-column of a cell.
/// Rows 0-2 are the historic 6-dot block; row 3 is the 8-dot extension, which
/// is why the fourth entry is not adjacent to the others.
const LEFT: [u8; 4] = [0x01, 0x02, 0x04, 0x40];
const RIGHT: [u8; 4] = [0x08, 0x10, 0x20, 0x80];

/// The wave's height at one sub-column, as a dot row in `0..4`.
///
/// A triangle rather than a sine: quantized to four rows the two are
/// indistinguishable, and integer-only keeps every frame bit-identical across
/// platforms — which is what lets the tests pin exact strings.
fn path_row(x: usize) -> usize {
    let half = WAVELENGTH / 2;
    let pos = x % WAVELENGTH;
    let climb = if pos < half {
        pos
    } else {
        WAVELENGTH - 1 - pos
    };
    (climb * 3 / (half - 1)).min(3)
}

/// Turn lit dot-bits into the `WIDTH` braille chars they spell.
fn render(cells: [u8; WIDTH]) -> String {
    cells
        .iter()
        .map(|bits| char::from_u32(0x2800 + *bits as u32).expect("braille block"))
        .collect()
}

/// The snake at rest: stretched flat along the middle of the strip.
///
/// Its own shape rather than an arbitrary frame of the animation. A snake
/// stopped mid-slither reads as one that is about to keep going; lying flat
/// reads as settled, which is exactly the difference between "working" and
/// "waiting on you". It is also the old resting motif — a straight line — in
/// the new alphabet.
pub fn at_rest() -> String {
    let mut cells = [0u8; WIDTH];
    // Ends left dark so the line reads as drawn *between* margins, not as
    // running off both edges.
    for x in 1..COLUMNS - 1 {
        let (col, row) = (if x.is_multiple_of(2) { LEFT } else { RIGHT }, 1);
        cells[x / 2] |= col[row];
    }
    render(cells)
}

/// Light the dot at (`row`, `col`); out-of-range dots are dropped, which lets a
/// pose reach a snout one column past the head without bounds-checking itself.
fn dot(g: &mut Grid, row: usize, col: usize) {
    if row < 4 && col < COLUMNS {
        g[row][col] = true;
    }
}

/// Light a vertical run in one sub-column, rows `r0..=r1` — a neck.
fn vrun(g: &mut Grid, col: usize, r0: usize, r1: usize) {
    for r in r0..=r1 {
        dot(g, r, col);
    }
}

/// The frame reflected left↔right about the strip's center. "Look left" is
/// exactly "look right" mirrored, so the two glances are guaranteed symmetric.
fn mirror(g: &Grid) -> Grid {
    let mut m = [[false; COLUMNS]; 4];
    for (r, row) in g.iter().enumerate() {
        for (c, &lit) in row.iter().enumerate() {
            if lit {
                m[r][COLUMNS - 1 - c] = true;
            }
        }
    }
    m
}

/// Pack a dot grid into the `WIDTH` braille cells it spells.
fn cells_of(g: &Grid) -> [u8; WIDTH] {
    let mut cells = [0u8; WIDTH];
    for (r, row) in g.iter().enumerate() {
        for (c, &lit) in row.iter().enumerate() {
            if lit {
                let bits = if c.is_multiple_of(2) { LEFT } else { RIGHT };
                cells[c / 2] |= bits[r];
            }
        }
    }
    cells
}

/// The swimming snake at `step`, heading `leftward` (head toward the low edge)
/// or rightward. The wave is fixed; the body slides along it head-first,
/// thinning to a single dot at the tail. `leftward` is the mirror traversal of
/// the rightward one, so both directions ride the same crest.
fn travel(step: u64, leftward: bool) -> Grid {
    let mut g = [[false; COLUMNS]; 4];
    let s = (step % COLUMNS as u64) as usize;
    let head = if leftward { COLUMNS - 1 - s } else { s };
    for segment in 0..BODY {
        // Walking back from the head wraps the body around the strip's end, so
        // the snake leaves one edge as it enters the other.
        let x = if leftward {
            (head + segment) % COLUMNS
        } else {
            (head + COLUMNS - segment) % COLUMNS
        };
        let row = path_row(x);
        // The front half is two dots thick and the tail one, which is what
        // gives the body a direction to travel in.
        let thickness = if segment < BODY / 2 { 2 } else { 1 };
        for d in 0..thickness {
            dot(&mut g, row + d, x);
        }
    }
    g
}

// The head, reared up in the middle: a periscope that springs straight up out
// of the strip (`pose_nub`→`pose_center`) and swivels to glance (`look`). No
// flat resting body — it rises from the center and drops back to swimming.

fn pose_nub() -> Grid {
    let mut g = [[false; COLUMNS]; 4];
    dot(&mut g, 3, 11);
    dot(&mut g, 3, 12);
    g
}
fn pose_rise1() -> Grid {
    let mut g = [[false; COLUMNS]; 4];
    vrun(&mut g, 11, 2, 3);
    vrun(&mut g, 12, 2, 3);
    g
}
fn pose_rise2() -> Grid {
    let mut g = [[false; COLUMNS]; 4];
    vrun(&mut g, 11, 1, 3);
    vrun(&mut g, 12, 2, 3);
    dot(&mut g, 0, 11);
    dot(&mut g, 0, 12);
    g
}
fn pose_rise3() -> Grid {
    let mut g = [[false; COLUMNS]; 4];
    vrun(&mut g, 11, 1, 3);
    dot(&mut g, 0, 11);
    dot(&mut g, 0, 12);
    dot(&mut g, 1, 11);
    g
}
/// The peak of the spring — a touch of overshoot before it settles.
fn pose_peak() -> Grid {
    let mut g = [[false; COLUMNS]; 4];
    vrun(&mut g, 11, 2, 3);
    dot(&mut g, 0, 11);
    dot(&mut g, 0, 12);
    g
}
/// Head up, facing forward — the settle between the spring and the first glance.
fn pose_center() -> Grid {
    let mut g = [[false; COLUMNS]; 4];
    vrun(&mut g, 11, 2, 3);
    dot(&mut g, 0, 11);
    dot(&mut g, 0, 12);
    dot(&mut g, 1, 11);
    dot(&mut g, 1, 12);
    g
}
/// The head cocked to glance right; `look(false)` mirrors it to glance left.
fn look(right: bool) -> Grid {
    let mut g = [[false; COLUMNS]; 4];
    vrun(&mut g, 11, 2, 3);
    dot(&mut g, 1, 12);
    dot(&mut g, 0, 13);
    dot(&mut g, 0, 14);
    dot(&mut g, 1, 13);
    dot(&mut g, 1, 14);
    if right {
        g
    } else {
        mirror(&g)
    }
}

/// The spring-up, frame by frame (`0..RISE`): nub breaks the surface, the neck
/// climbs, the head overshoots and settles.
fn rise(i: u64) -> Grid {
    match i {
        0 | 1 => pose_nub(),
        2 | 3 => pose_rise1(),
        4 | 5 => pose_rise2(),
        6 | 7 => pose_rise3(),
        8 => pose_peak(),
        _ => pose_center(),
    }
}

/// The drop back down (`0..FALL`): the spring-up run in reverse, to the nub.
fn fall(j: u64) -> Grid {
    match j {
        0 | 1 => pose_rise3(),
        2 | 3 => pose_rise2(),
        4 | 5 => pose_rise1(),
        _ => pose_nub(),
    }
}

/// One head-up interlude at local tick `i` (`0..INTERLUDE`): spring up, hold the
/// first glance a second, hold the second glance a second, drop back down.
/// `right_first` chooses which way it looks first — right after the leftward
/// swim, left after the rightward one (the inverse the second time round).
fn interlude(i: u64, right_first: bool) -> Grid {
    if i < RISE {
        rise(i)
    } else if i < RISE + LOOK {
        look(right_first)
    } else if i < RISE + 2 * LOOK {
        look(!right_first)
    } else {
        fall(i - (RISE + 2 * LOOK))
    }
}

/// The frame for the whole-turn clock `work`: where we are in the `CYCLE`.
fn frame(work: u64) -> Grid {
    let c = work % CYCLE;
    if c < TRAVEL {
        travel(c, true) // swim right → left
    } else if c < TRAVEL + INTERLUDE {
        interlude(c - TRAVEL, true) // glance right, then left
    } else if c < 2 * TRAVEL + INTERLUDE {
        travel(c - (TRAVEL + INTERLUDE), false) // swim left → right
    } else {
        interlude(c - (2 * TRAVEL + INTERLUDE), false) // glance left, then right
    }
}

/// The animation's current frame: `WIDTH` braille chars.
///
/// Phases that are *not* running a turn show `at_rest` rather than animating.
/// Idle deliberately schedules no wakeups (see `hotl::tui`), and a blocked
/// prompt that kept moving would read as progress when the whole point is that
/// nothing is happening until you answer. `work` is the whole-turn clock
/// (`State::work_ticks`), which advances across every running sub-phase, so the
/// cycle is unbroken by the thinking→writing→tool churn and restarts each turn.
pub fn snake(phase: &Phase, work: u64) -> String {
    let running = matches!(
        phase,
        Phase::Sampling { .. } | Phase::Streaming { .. } | Phase::Tool { .. }
    );
    if !running {
        return at_rest();
    }
    render(cells_of(&frame(work)))
}

/// The phase's gradient, as the two theme slots it sweeps between.
///
/// Both ends are palette slots on purpose: this is the whole reason the
/// animation tracks the theme. The choices read as what they mean — idle
/// rests on `idle`, a blocked prompt lands on `blocked`, a tool warms toward
/// `active`, and thinking climbs out of `faint` toward `accent`.
pub fn ramp_ends(phase: &Phase, p: &Palette) -> (Color, Color) {
    match phase {
        Phase::Idle => (p.faint, p.idle),
        Phase::Sampling { .. } => (p.faint, p.accent),
        Phase::Streaming { .. } => (p.accent, p.ink),
        Phase::Tool { .. } => (p.accent, p.active),
        Phase::WaitingAsk { .. } | Phase::WaitingQuestion { .. } | Phase::WaitingEgress { .. } => {
            (p.faint, p.blocked)
        }
    }
}

/// The strip's colors, cell by cell: the phase gradient sampled across
/// `WIDTH`.
pub fn snake_ramp(phase: &Phase, p: &Palette) -> Vec<Color> {
    let (a, b) = ramp_ends(phase, p);
    hotl_theme::ramp(a, b, WIDTH)
}

/// Everything on the strip after the snake. Rendered as one span in the
/// phase's text color, so it is kept separate from the gradient-lit body.
///
/// Empty only at idle before the first turn with no model known — the view
/// drops the separator rather than leaving a dangling space.
pub fn strip_text(state: &State) -> String {
    let secs = |ticks: u64| ticks / TICK_HZ;
    let base = match &state.phase {
        // Idle is the only phase with room to spare, and the only one where
        // "which model is this?" is still an open question — every other arm
        // is already reporting on a turn that model is running.
        Phase::Idle => {
            // Pre-open (0033 Task 8b): no session behind the composer yet —
            // the strip says so quietly. `mode` is never empty once open.
            if state.mode.is_empty() {
                return "starting…".to_string();
            }
            let mut parts = Vec::new();
            let model = hotl_types::bare_model(&state.model);
            if !model.is_empty() {
                parts.push(model.to_string());
            }
            if let Some(usage) = &state.usage_line {
                parts.push(usage.clone());
            }
            // Undo-point chip (0035 decision 11): opacity must not be
            // silence — the strip says whether `hotl undo` has a restore
            // point right now.
            if let Some(undo) = &state.undo_status {
                parts.push(format!("undo {undo}"));
            }
            parts.join(" · ")
        }
        Phase::Sampling { ticks } => format!("thinking · {}s · esc to interrupt", secs(*ticks)),
        Phase::Streaming { ticks, chars } => format!(
            "writing · ~{} tok · {}s · esc to interrupt",
            chars / 4,
            secs(*ticks)
        ),
        Phase::Tool { name, ticks } => format!("{name} · {}s · esc to interrupt", secs(*ticks)),
        Phase::WaitingAsk { .. } | Phase::WaitingQuestion { .. } => "waiting on you".to_string(),
        // Named on the strip, not just "waiting on you": the difference from
        // the tool ask is the whole point of the prompt.
        Phase::WaitingEgress { .. } => "waiting on you · network".to_string(),
    };
    let mut out = base;
    for suffix in [todos_summary(&state.todos), goal_summary(state)]
        .into_iter()
        .flatten()
    {
        if out.is_empty() {
            out = suffix;
        } else {
            out = format!("{out} · {suffix}");
        }
    }
    out
}

/// The goal's compact strip suffix: `◎ /goal active · 3m`. `None` when no
/// goal is set. Minutes come from the goal's own tick clock, which advances
/// only while a turn runs — exactly the time the loop is spending.
fn goal_summary(state: &State) -> Option<String> {
    state
        .goal
        .as_ref()
        .map(|_| format!("◎ /goal active · {}m", state.goal_ticks / (60 * TICK_HZ)))
}

/// Snake and text as one plain string — what the strip reads as, minus color.
/// The view renders the two parts separately (only the snake is gradient-lit);
/// this is the form tests pin and the form any non-styled consumer wants.
pub fn strip_line(state: &State) -> String {
    let snake = snake(&state.phase, state.work_ticks);
    match strip_text(state) {
        t if t.is_empty() => snake,
        t => format!("{snake} {t}"),
    }
}

/// The todo checklist's compact strip suffix: `"2/5 todos"`, or — while
/// exactly one item is `in_progress` — `"2/5 · wiring the gate"` (its
/// `active_form`, falling back to `content`). `None` when the list is empty:
/// nothing rides the strip until there's something to show progress on.
fn todos_summary(todos: &[hotl_tools::todo::Todo]) -> Option<String> {
    if todos.is_empty() {
        return None;
    }
    use hotl_tools::todo::TodoStatus;
    let done = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let total = todos.len();
    match todos.iter().find(|t| t.status == TodoStatus::InProgress) {
        Some(t) => Some(format!(
            "{done}/{total} {}",
            t.active_form.as_deref().unwrap_or(&t.content)
        )),
        None => Some(format!("{done}/{total} todos")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_tools::todo::{Todo, TodoStatus};

    fn todo(content: &str, status: TodoStatus, active_form: Option<&str>) -> Todo {
        Todo {
            content: content.into(),
            status,
            active_form: active_form.map(str::to_string),
        }
    }

    /// The resting shape every non-running phase shows. Pinned exactly by
    /// `the_snake_rests_flat_and_travels_when_a_turn_runs`; spelled once here
    /// so the composition tests below assert composition, not braille.
    fn still() -> String {
        at_rest()
    }

    /// The idle strip for `State::test_default`, whose model is `test-model` —
    /// the baseline every idle assertion below builds on.
    fn resting(suffix: &str) -> String {
        match suffix {
            "" => format!("{} test-model", still()),
            s => format!("{} test-model · {s}", still()),
        }
    }

    #[test]
    fn strip_line_carries_progress_and_the_active_items_text() {
        let mut s = State::test_default();
        assert_eq!(strip_line(&s), resting(""), "no todos: unchanged strip");

        s.todos = vec![
            todo("done thing", TodoStatus::Completed, None),
            todo(
                "wire the gate",
                TodoStatus::InProgress,
                Some("wiring the gate"),
            ),
            todo("write docs", TodoStatus::Pending, None),
        ];
        let line = strip_line(&s);
        assert!(line.contains("1/3"), "progress count: {line}");
        assert!(line.contains("wiring the gate"), "active item text: {line}");

        // No item in progress: falls back to a bare count, no item text.
        s.todos = vec![
            todo("done thing", TodoStatus::Completed, None),
            todo("write docs", TodoStatus::Pending, None),
        ];
        assert_eq!(strip_line(&s), resting("1/2 todos"));

        // Cleared list: strip goes back to exactly the no-todos baseline.
        s.todos.clear();
        assert_eq!(strip_line(&s), resting(""));
    }

    #[test]
    fn the_idle_strip_names_the_model_without_its_provider_prefix() {
        let mut s = State::new(true, "anthropic/claude-opus-5".into());
        assert_eq!(strip_line(&s), format!("{} claude-opus-5", still()));

        // A mid-session fallback re-seeds `state.model`; the strip follows it,
        // so the name on screen is the model that will run the next turn.
        s.model = "anthropic/claude-haiku-4-5-20251001".into();
        assert!(
            strip_line(&s).contains("claude-haiku-4-5-20251001"),
            "the strip must track the live model: {}",
            strip_line(&s)
        );

        // No handshake yet: no name, and no orphan separator either.
        s.model = String::new();
        assert_eq!(strip_line(&s), still());
    }

    #[test]
    fn the_snake_rests_and_runs_the_travel_look_cycle() {
        // The whole-turn clock drives one cycle; its landmarks pinned once.
        // `Sampling`'s own `ticks` no longer feed the animation — the second
        // argument (the work clock) does — so it stays 0 here throughout.
        let run = Phase::Sampling { ticks: 0 };

        // Rest is unchanged: a flat body along one dot row, ends dark.
        assert_eq!(at_rest(), "⠐⠒⠒⠒⠒⠒⠒⠒⠒⠒⠒⠂");

        // Swim right → left …
        assert_eq!(snake(&run, 0), "⠛⠳⢦⢄⡠⠔⠂⠀⠀⠀⠀⢀");
        assert_eq!(snake(&run, 1), "⠛⠳⠦⢄⡠⠔⠀⠀⠀⠀⠀⣄");
        // … then the head springs up in the middle and glances right, then left …
        assert_eq!(snake(&run, TRAVEL), "⠀⠀⠀⠀⠀⢀⡀⠀⠀⠀⠀⠀");
        assert_eq!(snake(&run, TRAVEL + RISE), "⠀⠀⠀⠀⠀⢠⠚⠃⠀⠀⠀⠀");
        assert_eq!(snake(&run, TRAVEL + RISE + LOOK), "⠀⠀⠀⠀⠘⠓⡄⠀⠀⠀⠀⠀");
        // … then swims back left → right …
        assert_eq!(snake(&run, TRAVEL + INTERLUDE), "⠃⠀⠀⠀⠀⠐⠊⠉⠉⠳⢦⣄");
        // … and glances in the inverse order the second time: left, then right.
        let second = 2 * TRAVEL + INTERLUDE;
        assert_eq!(snake(&run, second + RISE), "⠀⠀⠀⠀⠘⠓⡄⠀⠀⠀⠀⠀");
        assert_eq!(snake(&run, second + RISE + LOOK), "⠀⠀⠀⠀⠀⢠⠚⠃⠀⠀⠀⠀");

        // Every frame is exactly WIDTH cells — the strip's text never shifts
        // left or right, at any point in the cycle.
        for work in [0u64, 1, TRAVEL - 1, TRAVEL, TRAVEL + 5, CYCLE - 1, 10_000] {
            let f = snake(&run, work);
            assert_eq!(f.chars().count(), WIDTH, "width at {work}: {f}");
        }

        // The cycle is exactly CYCLE ticks: a full loop lands on itself.
        assert_eq!(snake(&run, CYCLE), snake(&run, 0));
        assert_eq!(
            snake(&run, CYCLE + TRAVEL + RISE),
            snake(&run, TRAVEL + RISE)
        );
    }

    #[test]
    fn only_a_running_turn_animates() {
        // Idle and every blocked prompt lie flat no matter the clock, so motion
        // on the strip always means the turn is moving.
        let ask = |req_id| Phase::WaitingAsk {
            req_id,
            summary: "s".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        for work in [0u64, 3, TRAVEL, 99_999] {
            assert_eq!(snake(&ask(1), work), still(), "a halted loop never moves");
            assert_eq!(snake(&Phase::Idle, work), still());
        }
        assert_ne!(
            still(),
            snake(&Phase::Sampling { ticks: 0 }, 0),
            "resting must not be mistakable for the first frame of working"
        );

        // …and a running turn does move: consecutive travel ticks differ.
        assert_ne!(
            snake(&Phase::Sampling { ticks: 0 }, 4),
            snake(&Phase::Sampling { ticks: 0 }, 5)
        );
    }

    #[test]
    fn every_frame_reads_as_a_creature_not_a_full_or_empty_strip() {
        // A snake, not a ripple and not a blank: across the whole cycle every
        // frame lights some cells and leaves some dark — a body with a gap, or a
        // head with room around it — never the full strip and never nothing.
        let run = Phase::Sampling { ticks: 0 };
        for work in 0..CYCLE {
            let f = snake(&run, work);
            let dark = f.chars().filter(|c| *c == '\u{2800}').count();
            assert!(dark > 0, "frame {work} fills the whole strip: {f}");
            assert!(dark < WIDTH, "frame {work} is entirely blank");
        }
    }

    #[test]
    fn each_phase_lights_the_snake_from_its_own_theme_slots() {
        let p = Palette::default();
        // Every end is a palette slot, so a preset or a single-slot override
        // recolors the animation with it. INVARIANT: no hardcoded RGB here.
        assert_eq!(ramp_ends(&Phase::Idle, &p), (p.faint, p.idle));
        assert_eq!(
            ramp_ends(&Phase::Sampling { ticks: 0 }, &p),
            (p.faint, p.accent)
        );
        assert_eq!(
            ramp_ends(&Phase::Streaming { ticks: 0, chars: 0 }, &p),
            (p.accent, p.ink)
        );
        assert_eq!(
            ramp_ends(
                &Phase::Tool {
                    name: "bash".into(),
                    ticks: 0
                },
                &p
            ),
            (p.accent, p.active)
        );
        assert_eq!(
            ramp_ends(
                &Phase::WaitingQuestion {
                    req_id: 1,
                    header: "h".into(),
                    prompt: "p".into(),
                    options: Vec::new(),
                    input: String::new(),
                },
                &p
            ),
            (p.faint, p.blocked)
        );

        // One color per column, endpoints exact — the sweep starts and ends
        // on the slots themselves, never near them.
        let ramp = snake_ramp(&Phase::Sampling { ticks: 0 }, &p);
        assert_eq!(ramp.len(), WIDTH);
        assert_eq!(ramp.first(), Some(&p.faint));
        assert_eq!(ramp.last(), Some(&p.accent));
    }

    #[test]
    fn strip_formats_pin_exact_strings() {
        let mut s = State::new(true, "m".into());
        let w = still();
        assert_eq!(strip_line(&s), format!("{w} m"));
        s.usage_line = Some("120 in · 45 out tok".into());
        assert_eq!(strip_line(&s), format!("{w} m · 120 in · 45 out tok"));

        // The per-phase `ticks` still drive the "· 1s ·" elapsed readout (off
        // TICK_HZ), while the animation rides the separate work clock.
        s.phase = Phase::Sampling { ticks: TICK_HZ };
        assert_eq!(
            strip_line(&s),
            format!(
                "{} thinking · 1s · esc to interrupt",
                snake(&s.phase, s.work_ticks)
            )
        );
        s.phase = Phase::Streaming {
            ticks: 2 * TICK_HZ,
            chars: 200,
        };
        assert_eq!(
            strip_text(&s),
            "writing · ~50 tok · 2s · esc to interrupt",
            "chars are shown as ~tokens, seconds as whole seconds"
        );
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 4,
        };
        assert_eq!(strip_text(&s), "bash · 0s · esc to interrupt");
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "s".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        assert_eq!(strip_line(&s), format!("{w} waiting on you"));
    }

    /// 0034: an active goal rides the strip as its own suffix — after the
    /// todo summary when both are present — with minutes off its own clock.
    #[test]
    fn the_goal_suffix_rides_the_strip_with_minutes() {
        let mut s = State::new(true, "m".into());
        s.goal = Some("all tests pass".into());
        assert_eq!(strip_text(&s), "m · ◎ /goal active · 0m");
        s.goal_ticks = 3 * 60 * TICK_HZ;
        assert_eq!(strip_text(&s), "m · ◎ /goal active · 3m");
        s.todos = vec![todo("wire the gate", TodoStatus::Pending, None)];
        assert_eq!(strip_text(&s), "m · 0/1 todos · ◎ /goal active · 3m");
        s.goal = None;
        assert_eq!(strip_text(&s), "m · 0/1 todos", "no goal, no suffix");
    }
}
