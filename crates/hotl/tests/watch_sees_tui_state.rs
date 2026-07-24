//! What `hotl watch` actually sees when the console TUI is blocked on you.
//!
//! `watch` reads a pane through two keyholes: the terminal title tmux records
//! as `#{pane_title}`, and the last screen rows `capture-pane` returns. Both
//! are taken from the real TUI here — the title from the `Cmd::SetTitle` the
//! Elm core emits, the rows from a real `view` render — so drift in either
//! one fails this test instead of silently muting watch's ping.

use hotl_theme::Palette;
use hotl_tui::app::{update, Cmd, Msg, State, TranscriptItem};
use hotl_tui::view::view;
use hotl_types::{Question, QuestionOption};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use watch_types::Status;

/// tmux's `#{pane_title}`: whatever the app last set with OSC 0/2.
fn pane_title(cmds: &[Cmd]) -> String {
    cmds.iter()
        .find_map(|c| match c {
            Cmd::SetTitle(t) => Some(t.clone()),
            _ => None,
        })
        .expect("a phase change retitles the terminal")
}

/// What `watch_tmux::capture_pane` hands the detector: the visible screen,
/// blank lines dropped, last 15 rows kept.
fn captured_tail(state: &State) -> String {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| view(state, &Palette::default(), f))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .filter(|r| !r.trim().is_empty())
        .collect();
    rows[rows.len().saturating_sub(15)..].join("\n")
}

fn seen_by_watch(state: &State, cmds: &[Cmd]) -> Status {
    watch_tmux::classify("hotl", &pane_title(cmds), &captured_tail(state))
}

fn session() -> State {
    let mut state = State::new(true, "m".into());
    state.session_name = Some("fix-auth".into());
    state
}

fn ask(state: &mut State) -> Vec<Cmd> {
    update(
        state,
        Msg::PermissionRequest {
            req_id: 1,
            summary: "bash: cargo test".into(),
            protected_why: None,
        },
    )
}

#[test]
fn watch_sees_blocked_while_the_permission_card_is_up() {
    let mut state = session();
    let cmds = ask(&mut state);
    assert_eq!(seen_by_watch(&state, &cmds), Status::Blocked);
}

/// The card is centered over the transcript, so a long session pushes it off
/// the top of the 15-row tail. The title is what survives that.
#[test]
fn watch_sees_blocked_even_when_the_card_scrolls_out_of_the_tail() {
    let mut state = session();
    for i in 0..40 {
        state.transcript.push(TranscriptItem::Assistant {
            text: format!("line {i} of a long session"),
        });
    }
    let cmds = ask(&mut state);
    assert_eq!(seen_by_watch(&state, &cmds), Status::Blocked);
}

#[test]
fn watch_sees_blocked_while_a_question_is_up() {
    let mut state = session();
    let cmds = update(
        &mut state,
        Msg::QuestionRequest {
            req_id: 2,
            question: Question {
                header: "Scope".into(),
                prompt: "How far should this go?".into(),
                options: vec![
                    QuestionOption {
                        label: "MVP".into(),
                        description: None,
                    },
                    QuestionOption {
                        label: "Full".into(),
                        description: None,
                    },
                ],
                multi: false,
            },
        },
    );
    assert_eq!(seen_by_watch(&state, &cmds), Status::Blocked);
}

#[test]
fn watch_sees_working_once_the_ask_is_answered() {
    let mut state = session();
    ask(&mut state);
    let cmds = update(
        &mut state,
        Msg::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('y'),
            crossterm::event::KeyModifiers::NONE,
        )),
    );
    assert_eq!(seen_by_watch(&state, &cmds), Status::Working);
}

#[test]
fn watch_sees_idle_when_the_turn_finishes() {
    let mut state = session();
    let cmds = update(
        &mut state,
        Msg::PromptResult {
            outcome_kind: "end_turn".into(),
            outcome_text: None,
            usage: serde_json::json!({}),
        },
    );
    assert_eq!(seen_by_watch(&state, &cmds), Status::Idle);
}
