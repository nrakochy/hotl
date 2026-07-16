// End-to-end: a raw Ctrl-k / Ctrl-j keystroke flows through decode_key and
// update to a pane switch, while plain j/k stay list navigation.
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
fn ctrl_k_switches_pane_never_moves_list() {
    let mut pending = None;
    let msg = decode_key(KeyCode::Char('k'), KeyModifiers::CONTROL, true, &mut pending);
    assert_eq!(msg, Msg::CtrlNav(Dir::Up));

    let mut s = AppState::new(true, true);
    update(&mut s, Msg::Scanned(Ok((vec![obs("%1"), obs("%2")], vec![]))));
    s.move_bottom(); // cursor = 1, mid/bottom of list
    assert_eq!(update(&mut s, msg), vec![Cmd::SelectPane(Dir::Up)]);
    assert_eq!(s.cursor, 1, "Ctrl-k does not touch the list cursor");
}

#[test]
fn plain_j_moves_list_not_pane() {
    let mut pending = None;
    let msg = decode_key(KeyCode::Char('j'), KeyModifiers::NONE, true, &mut pending);
    assert_eq!(msg, Msg::Down);

    let mut s = AppState::new(true, true);
    update(&mut s, Msg::Scanned(Ok((vec![obs("%1"), obs("%2")], vec![]))));
    assert_eq!(update(&mut s, msg), vec![]);
    assert_eq!(s.cursor, 1);
}
