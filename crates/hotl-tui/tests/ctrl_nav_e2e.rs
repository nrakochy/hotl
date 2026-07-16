// End-to-end: a raw Ctrl-k / Ctrl-j keystroke flows through decode_key and
// update to the right Cmd, matching the vim-tmux-navigator handoff contract.
use crossterm::event::{KeyCode, KeyModifiers};
use tui::{decode_key, update, AppState, Cmd, Msg};
use types::{Agent, AgentObservation, Dir, Location, LocationHandle, Source, Status};

fn obs(pane: &str) -> AgentObservation {
    AgentObservation {
        agent: Agent { name: "claude".into(), pid: 1, argv: "claude".into() },
        cwd: format!("/tmp/{pane}"),
        status: Status::Idle,
        status_line: None,
        location: Location {
            group: "g".into(),
            sub_group: None,
            handle: LocationHandle::Tmux { pane_id: pane.into(), session: "g".into(), window_index: 0 },
        },
        source: Source::Tmux,
    }
}

#[test]
fn ctrl_k_at_top_crosses_to_pane_above() {
    let mut pending = None;
    // Raw Ctrl-k as crossterm delivers it in raw mode.
    let msg = decode_key(KeyCode::Char('k'), KeyModifiers::CONTROL, true, &mut pending);
    assert_eq!(msg, Msg::CtrlNav(Dir::Up));

    let mut s = AppState::new(true, true);
    update(&mut s, Msg::Scanned(Ok((vec![obs("%1"), obs("%2")], vec![]))));
    // cursor starts at top (0): Ctrl-k should hand off to the pane above.
    assert_eq!(update(&mut s, msg), vec![Cmd::SelectPane(Dir::Up)]);
}

#[test]
fn ctrl_j_mid_list_scrolls_not_crosses() {
    let mut pending = None;
    let msg = decode_key(KeyCode::Char('j'), KeyModifiers::CONTROL, true, &mut pending);
    let mut s = AppState::new(true, true);
    update(&mut s, Msg::Scanned(Ok((vec![obs("%1"), obs("%2")], vec![]))));
    // At top with 2 items, Ctrl-j moves down inside the list (no pane switch).
    assert_eq!(update(&mut s, msg), vec![]);
    assert_eq!(s.cursor, 1);
}
