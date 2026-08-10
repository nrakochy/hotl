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

/// The animation's current frame: `WIDTH` braille chars.
///
/// Phases that are *not* running a turn show `at_rest` rather than animating.
/// Idle deliberately schedules no wakeups (see `hotl::tui`), and a blocked
/// prompt that kept moving would read as progress when the whole point is that
/// nothing is happening until you answer.
pub fn snake(phase: &Phase) -> String {
    let step = match phase {
        Phase::Sampling { ticks } | Phase::Streaming { ticks, .. } | Phase::Tool { ticks, .. } => {
            *ticks
        }
        Phase::Idle
        | Phase::WaitingAsk { .. }
        | Phase::WaitingQuestion { .. }
        | Phase::WaitingEgress { .. } => return at_rest(),
    };
    let mut cells = [0u8; WIDTH];
    let head = (step % COLUMNS as u64) as usize;
    for segment in 0..BODY {
        // Walking back from the head wraps the body around the strip's end,
        // so the snake leaves one edge as it enters the other.
        let x = (head + COLUMNS - segment) % COLUMNS;
        let row = path_row(x);
        let col = if x.is_multiple_of(2) { LEFT } else { RIGHT };
        // The front half is two dots thick and the tail one, which is what
        // gives the body a direction to travel in.
        let thickness = if segment < BODY / 2 { 2 } else { 1 };
        for dot in 0..thickness {
            if let Some(bit) = col.get(row + dot) {
                cells[x / 2] |= bit;
            }
        }
    }
    render(cells)
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
            let mut parts = Vec::new();
            let model = hotl_types::bare_model(&state.model);
            if !model.is_empty() {
                parts.push(model.to_string());
            }
            if let Some(usage) = &state.usage_line {
                parts.push(usage.clone());
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
    match (base.is_empty(), todos_summary(&state.todos)) {
        (_, None) => base,
        (true, Some(summary)) => summary,
        (false, Some(summary)) => format!("{base} · {summary}"),
    }
}

/// Snake and text as one plain string — what the strip reads as, minus color.
/// The view renders the two parts separately (only the snake is gradient-lit);
/// this is the form tests pin and the form any non-styled consumer wants.
pub fn strip_line(state: &State) -> String {
    let snake = snake(&state.phase);
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
    fn the_snake_rests_flat_and_travels_when_a_turn_runs() {
        // The exact shapes, pinned once. At rest the body lies flat along one
        // dot row, ends dark; running, it undulates and moves one sub-column
        // per tick, which is half a cell — so consecutive frames share a lot.
        assert_eq!(at_rest(), "⠐⠒⠒⠒⠒⠒⠒⠒⠒⠒⠒⠂");
        assert_eq!(snake(&Phase::Sampling { ticks: 0 }), "⠃⠀⠀⠀⠀⠐⠊⠉⠉⠳⢦⣄");
        assert_eq!(snake(&Phase::Sampling { ticks: 1 }), "⠛⠀⠀⠀⠀⠀⠊⠉⠉⠱⢦⣄");
        assert_eq!(snake(&Phase::Sampling { ticks: 2 }), "⠛⠃⠀⠀⠀⠀⠈⠉⠉⠑⢦⣄");

        // Every frame is exactly WIDTH cells — the strip's text never shifts
        // left or right as the body wraps around the end.
        for ticks in [0u64, 1, 7, 23, 24, 29, 30, 1_000] {
            let f = snake(&Phase::Sampling { ticks });
            assert_eq!(f.chars().count(), WIDTH, "width at {ticks}: {f}");
        }

        // The head laps the strip every COLUMNS ticks, and the path it walks
        // is fixed, so a full lap lands on exactly the same picture.
        assert_eq!(
            snake(&Phase::Sampling {
                ticks: COLUMNS as u64
            }),
            snake(&Phase::Sampling { ticks: 0 })
        );
    }

    #[test]
    fn only_a_running_turn_animates() {
        // Idle and every blocked prompt lie flat no matter the tick, so motion
        // on the strip always means the turn is moving.
        let ask = |ticks| Phase::WaitingAsk {
            req_id: ticks,
            summary: "s".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        for ticks in [0u64, 3, 99] {
            assert_eq!(snake(&ask(ticks)), still(), "a halted loop never moves");
        }
        assert_eq!(snake(&Phase::Idle), still());
        assert_ne!(
            still(),
            snake(&Phase::Sampling { ticks: 0 }),
            "resting must not be mistakable for the first frame of working"
        );

        // …and a running turn does move: consecutive ticks differ.
        assert_ne!(
            snake(&Phase::Sampling { ticks: 4 }),
            snake(&Phase::Sampling { ticks: 5 })
        );
    }

    #[test]
    fn the_body_is_continuous_and_has_a_head_and_a_tail() {
        // A snake, not a ripple: every frame lights some cells and leaves
        // others dark, so the body reads as one creature with a gap behind it.
        for ticks in 0..COLUMNS as u64 {
            let f = snake(&Phase::Sampling { ticks });
            let dark = f.chars().filter(|c| *c == '\u{2800}').count();
            assert!(
                dark > 0,
                "frame {ticks} fills the whole strip — no tail to see: {f}"
            );
            assert!(
                dark < WIDTH,
                "frame {ticks} is entirely blank: nothing to see"
            );
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

        // Seconds come off TICK_HZ, so these tick counts are rates, not magic.
        s.phase = Phase::Sampling { ticks: TICK_HZ };
        assert_eq!(
            strip_line(&s),
            format!(
                "{} thinking · 1s · esc to interrupt",
                snake(&Phase::Sampling { ticks: TICK_HZ })
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
}
