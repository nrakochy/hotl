//! Transcript scroll intent. Keys, the mouse wheel, and vim's `j`/`k` all
//! produce an `Intent`; `apply` is the single place `State::scroll` moves.
//! Indices are transcript-*item* indices, matching `view::render_transcript`'s
//! slicing — a page is ten items, not ten rows.

use crate::app::{Scroll, State};

/// One page, in transcript items.
pub const PAGE: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Up(usize),
    Down(usize),
    Top,
    Bottom,
}

// INVARIANT: `Scroll::Follow` is the only state that tracks new items, and
// scrolling to the newest item always lands back in `Follow` (never `At(len)`),
// so a subsequent append stays visible. Enforced by
// `scrolling_down_past_the_end_returns_to_follow`.
pub fn apply(state: &mut State, intent: Intent) {
    let len = state.transcript.len();
    let cur = match state.scroll {
        Scroll::Follow => len,
        Scroll::At(i) => i,
    };
    state.scroll = match intent {
        Intent::Top => Scroll::At(0),
        Intent::Bottom => Scroll::Follow,
        Intent::Up(n) => Scroll::At(cur.saturating_sub(n)),
        Intent::Down(n) => {
            let next = cur.saturating_add(n);
            if next >= len {
                Scroll::Follow
            } else {
                Scroll::At(next)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TranscriptItem;

    fn state_with(n: usize) -> State {
        let mut s = State::test_default();
        s.transcript = (0..n)
            .map(|i| TranscriptItem::Notice {
                text: i.to_string().into(),
            })
            .collect();
        s
    }

    #[test]
    fn page_up_from_follow_lands_a_page_above_the_end() {
        let mut s = state_with(30);
        apply(&mut s, Intent::Up(PAGE));
        assert_eq!(s.scroll, Scroll::At(20));
    }

    #[test]
    fn page_up_saturates_at_the_top_and_never_underflows() {
        let mut s = state_with(3);
        apply(&mut s, Intent::Up(PAGE));
        apply(&mut s, Intent::Up(PAGE));
        assert_eq!(s.scroll, Scroll::At(0));
    }

    #[test]
    fn scrolling_down_past_the_end_returns_to_follow() {
        let mut s = state_with(12);
        apply(&mut s, Intent::Up(PAGE)); // At(2)
        apply(&mut s, Intent::Down(PAGE));
        assert_eq!(s.scroll, Scroll::Follow, "must re-follow, never At(len)");
    }

    #[test]
    fn top_and_bottom_are_absolute() {
        let mut s = state_with(30);
        apply(&mut s, Intent::Top);
        assert_eq!(s.scroll, Scroll::At(0));
        apply(&mut s, Intent::Bottom);
        assert_eq!(s.scroll, Scroll::Follow);
    }

    #[test]
    fn scrolling_an_empty_transcript_is_a_no_op() {
        let mut s = state_with(0);
        apply(&mut s, Intent::Up(PAGE));
        assert_eq!(s.scroll, Scroll::At(0));
        apply(&mut s, Intent::Down(1));
        assert_eq!(s.scroll, Scroll::Follow);
    }
}
