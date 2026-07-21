//! Loop-motif activity animation. Pure frame lookup: ticks arrive at 8/sec
//! only while a turn runs (idle draws nothing and schedules no wakeups), so
//! `ticks % N` indexes the const frame arrays. Salience is visual only — no
//! bell, ever.

use crate::app::{Phase, State};

pub const RESTING: &str = "· ─ ·";
/// Sampling ticks 0-5 draw the loop stroke by stroke, then…
pub const DRAWING: [&str; 6] = ["╭", "╭─", "╭─╮", "╭─╮╯", "╭─╮─╯", "╭─╮╰─╯"];
/// …it rotates: `DRAWING` then `TURNING[(ticks - 6) % 4]`.
pub const TURNING: [&str; 4] = ["╭─╮╰─╯", "╭╮ ╰╯─", "─╮╭ ╯╰", "╮─╭╯ ╰"];
/// Tool: the dot circulates the loop.
pub const ORBIT: [&str; 4] = ["●─╮╰─╯", "╭●╮╰─╯", "╭─●╰─╯", "╭─╮╰●╯"];
/// WaitingAsk: halted, the gap is you.
pub const GAP: &str = "╭─╮╰ ╯";
pub const COIL: [&str; 4] = ["◜◝◟◞", "◜◝", "◜", "·"];

pub fn loop_glyph(phase: &Phase) -> &'static str {
    match phase {
        Phase::Idle => RESTING,
        Phase::Sampling { ticks } | Phase::Streaming { ticks, .. } => draw_then_turn(*ticks),
        Phase::Tool { ticks, .. } => ORBIT[(*ticks % 4) as usize],
        Phase::WaitingAsk { .. } => GAP,
        Phase::Compacting { ticks } => COIL[(*ticks % 4) as usize],
    }
}

fn draw_then_turn(ticks: u64) -> &'static str {
    match ticks {
        0..=5 => DRAWING[ticks as usize],
        t => TURNING[((t - 6) % 4) as usize],
    }
}

/// The full activity-strip text — the view renders this verbatim, so tests
/// pin the exact formats here.
pub fn strip_line(state: &State) -> String {
    let glyph = loop_glyph(&state.phase);
    let secs = |ticks: u64| ticks / 8;
    match &state.phase {
        Phase::Idle => match &state.usage_line {
            Some(usage) => format!("{RESTING} · {usage}"),
            None => RESTING.to_string(),
        },
        Phase::Sampling { ticks } => {
            format!("{glyph} thinking · {}s · esc to interrupt", secs(*ticks))
        }
        Phase::Streaming { ticks, chars } => {
            format!(
                "{glyph} writing · ~{} tok · {}s · esc to interrupt",
                chars / 4,
                secs(*ticks)
            )
        }
        Phase::Tool { name, ticks } => {
            format!("{glyph} {name} · {}s · esc to interrupt", secs(*ticks))
        }
        Phase::WaitingAsk { .. } => format!("{GAP} waiting on you"),
        Phase::Compacting { .. } => format!("{glyph} folding history…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_draws_then_turns() {
        for (i, frame) in DRAWING.iter().enumerate() {
            assert_eq!(loop_glyph(&Phase::Sampling { ticks: i as u64 }), *frame);
        }
        // Period 4 after the draw-in: ticks 6 and 10 land on the same frame.
        assert_eq!(loop_glyph(&Phase::Sampling { ticks: 6 }), TURNING[0]);
        assert_eq!(loop_glyph(&Phase::Sampling { ticks: 10 }), TURNING[0]);
    }

    #[test]
    fn ask_glyph_is_halted_gap() {
        for ticks in [0u64, 3, 99] {
            let phase = Phase::WaitingAsk {
                req_id: ticks,
                summary: "s".into(),
                protected_why: None,
                input: String::new(),
                denying: false,
            };
            assert_eq!(loop_glyph(&phase), GAP, "the gap never animates");
        }
    }

    #[test]
    fn strip_formats_pin_exact_strings() {
        let mut s = State::new(true, "m".into());
        assert_eq!(strip_line(&s), "· ─ ·");
        s.usage_line = Some("120 in · 45 out tok".into());
        assert_eq!(strip_line(&s), "· ─ · · 120 in · 45 out tok");
        s.phase = Phase::Sampling { ticks: 8 };
        assert_eq!(strip_line(&s), "─╮╭ ╯╰ thinking · 1s · esc to interrupt");
        s.phase = Phase::Streaming {
            ticks: 16,
            chars: 200,
        };
        assert_eq!(
            strip_line(&s),
            "─╮╭ ╯╰ writing · ~50 tok · 2s · esc to interrupt"
        );
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 4,
        };
        assert_eq!(strip_line(&s), "●─╮╰─╯ bash · 0s · esc to interrupt");
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "s".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
        };
        assert_eq!(strip_line(&s), "╭─╮╰ ╯ waiting on you");
        s.phase = Phase::Compacting { ticks: 1 };
        assert_eq!(strip_line(&s), "◜◝ folding history…");
    }
}
