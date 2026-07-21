use crossterm::event::{KeyCode, KeyModifiers};
use watch_types::{AgentObservation, Status};

#[derive(Debug, Default)]
pub struct AppState {
    pub agents: Vec<AgentObservation>,
    pub cursor: usize,
    pub status: String,
    pub should_quit: bool,
    pub ping_on_blocked: bool,
    pub vim_mode: bool,
    pub spinner_tick: u32,
}

impl AppState {
    pub fn new(ping_on_blocked: bool, vim_mode: bool) -> Self {
        AppState {
            status: "starting…".into(),
            ping_on_blocked,
            vim_mode,
            ..Default::default()
        }
    }

    fn id(o: &AgentObservation) -> String {
        match &o.location.handle {
            watch_types::LocationHandle::Tmux { pane_id, .. } => format!("tmux:{pane_id}"),
        }
    }

    pub fn selected(&self) -> Option<&AgentObservation> {
        self.agents.get(self.cursor)
    }

    pub fn move_down(&mut self) {
        if !self.agents.is_empty() && self.cursor + 1 < self.agents.len() {
            self.cursor += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_top(&mut self) {
        self.cursor = 0;
    }

    pub fn move_bottom(&mut self) {
        self.cursor = self.agents.len().saturating_sub(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Tick,
    Up,
    Down,
    Top,
    Bottom,
    Jump,
    // Ctrl-h/j/k/l: vim-tmux-navigator style. Move within the list when the
    // direction has somewhere to go; otherwise hand off to the neighboring
    // tmux pane. Left/right always hand off (the list is one-dimensional).
    CtrlNav(watch_types::Dir),
    Refresh,
    Quit,
    // Ok carries observations plus any per-surface partial-failure warnings.
    Scanned(Result<(Vec<AgentObservation>, Vec<String>), String>),
    Jumped(Result<String, String>),
    // A real (non-"no neighbor") pane-selection failure to show in the status bar.
    PaneSelectFailed(String),
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Scan,
    // Boxed: AgentObservation is large relative to the other variants, so
    // boxing keeps Cmd small (avoids clippy::large_enum_variant).
    Jump(Box<AgentObservation>),
    SelectPane(watch_types::Dir),
    Ping,
    Quit,
}

fn newly_blocked(prev: &[AgentObservation], next: &[AgentObservation]) -> bool {
    next.iter().any(|n| {
        n.status == Status::Blocked
            && prev
                .iter()
                .find(|p| AppState::id(p) == AppState::id(n))
                .is_none_or(|p| p.status != Status::Blocked)
    })
}

pub fn update(state: &mut AppState, msg: Msg) -> Vec<Cmd> {
    match msg {
        Msg::Tick | Msg::Refresh => vec![Cmd::Scan],
        Msg::Up => {
            state.move_up();
            vec![]
        }
        Msg::Down => {
            state.move_down();
            vec![]
        }
        Msg::Top => {
            state.move_top();
            vec![]
        }
        Msg::Bottom => {
            state.move_bottom();
            vec![]
        }
        Msg::Jump => match state.selected() {
            Some(o) => vec![Cmd::Jump(Box::new(o.clone()))],
            None => vec![],
        },
        // Ctrl-h/j/k/l always switch tmux panes; list nav is plain j/k.
        Msg::CtrlNav(dir) => vec![Cmd::SelectPane(dir)],
        Msg::Quit => {
            state.should_quit = true;
            vec![Cmd::Quit]
        }
        Msg::Scanned(Ok((next, warnings))) => {
            let ping = state.ping_on_blocked && newly_blocked(&state.agents, &next);
            let selected_id = state.selected().map(AppState::id);
            state.agents = next;
            state.cursor = selected_id
                .and_then(|id| state.agents.iter().position(|o| AppState::id(o) == id))
                .unwrap_or(0);
            let n = state.agents.len();
            state.status = if warnings.is_empty() {
                format!("{n} agent{}", if n == 1 { "" } else { "s" })
            } else {
                // A surface degraded; keep the healthy count visible but flag it.
                format!(
                    "{n} agent{} ({})",
                    if n == 1 { "" } else { "s" },
                    warnings.join("; ")
                )
            };
            if ping {
                vec![Cmd::Ping]
            } else {
                vec![]
            }
        }
        Msg::Scanned(Err(e)) => {
            state.status = format!("scan error: {e}");
            vec![]
        }
        Msg::Jumped(Ok(id)) => {
            state.status = format!("jumped to {id}");
            vec![]
        }
        Msg::Jumped(Err(e)) => {
            // Pane likely vanished; rescan rather than leaving a stale list.
            state.status = format!("jump failed ({e}) — rescanning");
            vec![Cmd::Scan]
        }
        Msg::PaneSelectFailed(e) => {
            state.status = format!("pane select failed: {e}");
            vec![]
        }
        Msg::Ignored => vec![],
    }
}

pub fn decode_key(
    code: KeyCode,
    mods: KeyModifiers,
    vim_mode: bool,
    pending: &mut Option<char>,
) -> Msg {
    let ctrl = mods.contains(KeyModifiers::CONTROL);

    if ctrl {
        *pending = None;
        return match code {
            KeyCode::Char('c') => Msg::Quit,
            KeyCode::Char('j') => Msg::CtrlNav(watch_types::Dir::Down),
            KeyCode::Char('k') => Msg::CtrlNav(watch_types::Dir::Up),
            KeyCode::Char('h') => Msg::CtrlNav(watch_types::Dir::Left),
            KeyCode::Char('l') => Msg::CtrlNav(watch_types::Dir::Right),
            _ => Msg::Ignored,
        };
    }

    if *pending == Some('g') {
        *pending = None;
        match code {
            KeyCode::Char('g') => return Msg::Top,
            KeyCode::Char('d') => return Msg::Jump,
            _ => {}
        }
    }

    match code {
        KeyCode::Down => return Msg::Down,
        KeyCode::Up => return Msg::Up,
        KeyCode::Enter => return Msg::Jump,
        KeyCode::Char('q') | KeyCode::Esc => return Msg::Quit,
        KeyCode::Char('r') => return Msg::Refresh,
        _ => {}
    }

    if vim_mode {
        match code {
            KeyCode::Char('j') => return Msg::Down,
            KeyCode::Char('k') => return Msg::Up,
            KeyCode::Char('G') => return Msg::Bottom,
            KeyCode::Char('g') => {
                *pending = Some('g');
                return Msg::Ignored;
            }
            _ => {}
        }
    }

    Msg::Ignored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use watch_types::{Agent, Location, LocationHandle, Source};

    fn obs(pane: &str, status: Status) -> AgentObservation {
        AgentObservation {
            agent: Agent {
                name: "claude".into(),
                pid: 1,
                argv: "claude".into(),
            },
            cwd: format!("/tmp/{pane}"),
            status,
            status_line: None,
            location: Location {
                group: "g".into(),
                sub_group: None,
                handle: LocationHandle::Tmux {
                    pane_id: pane.into(),
                    session: "g".into(),
                    window_index: 0,
                },
            },
            source: Source::Tmux,
        }
    }

    // Build a successful scan Msg with no partial-failure warnings.
    fn scanned(agents: Vec<AgentObservation>) -> Msg {
        Msg::Scanned(Ok((agents, vec![])))
    }

    #[test]
    fn tick_requests_scan() {
        let mut s = AppState::new(true, true);
        assert_eq!(update(&mut s, Msg::Tick), vec![Cmd::Scan]);
    }

    #[test]
    fn scanned_updates_and_counts() {
        let mut s = AppState::new(true, true);
        update(
            &mut s,
            scanned(vec![obs("%1", Status::Idle), obs("%2", Status::Idle)]),
        );
        assert_eq!(s.agents.len(), 2);
        assert_eq!(s.status, "2 agents");
    }

    #[test]
    fn scanned_partial_failure_keeps_agents_and_flags_status() {
        let mut s = AppState::new(true, true);
        let msg = Msg::Scanned(Ok((
            vec![obs("%1", Status::Idle)],
            vec!["surface x down".into()],
        )));
        update(&mut s, msg);
        assert_eq!(s.agents.len(), 1, "healthy agents still shown");
        assert!(
            s.status.contains("surface x down"),
            "warning surfaced: {}",
            s.status
        );
    }

    #[test]
    fn ping_on_transition_into_blocked() {
        let mut s = AppState::new(true, true);
        update(&mut s, scanned(vec![obs("%1", Status::Idle)]));
        let cmds = update(&mut s, scanned(vec![obs("%1", Status::Blocked)]));
        assert!(cmds.contains(&Cmd::Ping));
    }

    #[test]
    fn no_ping_while_staying_blocked() {
        let mut s = AppState::new(true, true);
        update(&mut s, scanned(vec![obs("%1", Status::Blocked)]));
        let cmds = update(&mut s, scanned(vec![obs("%1", Status::Blocked)]));
        assert!(!cmds.contains(&Cmd::Ping));
    }

    #[test]
    fn no_ping_when_disabled() {
        let mut s = AppState::new(false, true);
        update(&mut s, scanned(vec![obs("%1", Status::Idle)]));
        let cmds = update(&mut s, scanned(vec![obs("%1", Status::Blocked)]));
        assert!(!cmds.contains(&Cmd::Ping));
    }

    #[test]
    fn selection_follows_identity_across_scan() {
        let mut s = AppState::new(true, true);
        update(
            &mut s,
            scanned(vec![obs("%1", Status::Idle), obs("%2", Status::Idle)]),
        );
        s.move_down();
        assert_eq!(AppState::id(s.selected().unwrap()), "tmux:%2");
        update(&mut s, scanned(vec![obs("%2", Status::Idle)]));
        assert_eq!(s.cursor, 0);
        assert_eq!(AppState::id(s.selected().unwrap()), "tmux:%2");
    }

    #[test]
    fn jump_emits_cmd_for_selected() {
        let mut s = AppState::new(true, true);
        update(&mut s, scanned(vec![obs("%1", Status::Idle)]));
        match &update(&mut s, Msg::Jump)[..] {
            [Cmd::Jump(o)] => assert_eq!(AppState::id(o), "tmux:%1"),
            other => panic!("expected Jump, got {other:?}"),
        }
    }

    #[test]
    fn top_and_bottom_move_cursor_to_ends() {
        let mut s = AppState::new(true, true);
        update(
            &mut s,
            scanned(vec![
                obs("%1", Status::Idle),
                obs("%2", Status::Idle),
                obs("%3", Status::Idle),
            ]),
        );
        update(&mut s, Msg::Bottom);
        assert_eq!(s.cursor, 2);
        update(&mut s, Msg::Top);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn ctrl_nav_always_selects_neighbor_pane() {
        let mut s = AppState::new(true, true);
        update(
            &mut s,
            scanned(vec![obs("%1", Status::Idle), obs("%2", Status::Idle)]),
        );
        for dir in [
            watch_types::Dir::Up,
            watch_types::Dir::Down,
            watch_types::Dir::Left,
            watch_types::Dir::Right,
        ] {
            assert_eq!(
                update(&mut s, Msg::CtrlNav(dir)),
                vec![Cmd::SelectPane(dir)]
            );
            assert_eq!(s.cursor, 0, "Ctrl-nav never moves the list cursor");
        }
    }

    #[test]
    fn plain_jk_navigate_the_list() {
        let mut s = AppState::new(true, true);
        update(
            &mut s,
            scanned(vec![obs("%1", Status::Idle), obs("%2", Status::Idle)]),
        );
        assert_eq!(update(&mut s, Msg::Down), vec![]);
        assert_eq!(s.cursor, 1);
        assert_eq!(update(&mut s, Msg::Up), vec![]);
        assert_eq!(s.cursor, 0);
    }

    fn dk(code: KeyCode, mods: KeyModifiers, vim: bool, pending: &mut Option<char>) -> Msg {
        decode_key(code, mods, vim, pending)
    }

    #[test]
    fn arrows_and_enter_always_work() {
        let mut p = None;
        assert_eq!(
            dk(KeyCode::Down, KeyModifiers::NONE, false, &mut p),
            Msg::Down
        );
        assert_eq!(dk(KeyCode::Up, KeyModifiers::NONE, false, &mut p), Msg::Up);
        assert_eq!(
            dk(KeyCode::Enter, KeyModifiers::NONE, false, &mut p),
            Msg::Jump
        );
        assert_eq!(
            dk(KeyCode::Char('q'), KeyModifiers::NONE, false, &mut p),
            Msg::Quit
        );
        assert_eq!(
            dk(KeyCode::Char('r'), KeyModifiers::NONE, false, &mut p),
            Msg::Refresh
        );
    }

    #[test]
    fn ctrl_chords_decode_to_directional_nav_regardless_of_vim_mode() {
        let mut p = None;
        let c = KeyModifiers::CONTROL;
        assert_eq!(
            dk(KeyCode::Char('j'), c, false, &mut p),
            Msg::CtrlNav(watch_types::Dir::Down)
        );
        assert_eq!(
            dk(KeyCode::Char('k'), c, false, &mut p),
            Msg::CtrlNav(watch_types::Dir::Up)
        );
        assert_eq!(
            dk(KeyCode::Char('h'), c, false, &mut p),
            Msg::CtrlNav(watch_types::Dir::Left)
        );
        assert_eq!(
            dk(KeyCode::Char('l'), c, false, &mut p),
            Msg::CtrlNav(watch_types::Dir::Right)
        );
    }

    #[test]
    fn ctrl_c_quits() {
        let mut p = None;
        assert_eq!(
            dk(KeyCode::Char('c'), KeyModifiers::CONTROL, false, &mut p),
            Msg::Quit
        );
        assert_eq!(
            dk(KeyCode::Char('c'), KeyModifiers::CONTROL, true, &mut p),
            Msg::Quit
        );
    }

    #[test]
    fn vim_letters_only_when_vim_mode() {
        let mut p = None;
        let n = KeyModifiers::NONE;
        assert_eq!(dk(KeyCode::Char('j'), n, true, &mut p), Msg::Down);
        assert_eq!(dk(KeyCode::Char('k'), n, true, &mut p), Msg::Up);
        assert_eq!(dk(KeyCode::Char('G'), n, true, &mut p), Msg::Bottom);
        assert_eq!(dk(KeyCode::Char('j'), n, false, &mut p), Msg::Ignored);
        assert_eq!(dk(KeyCode::Char('k'), n, false, &mut p), Msg::Ignored);
        assert_eq!(dk(KeyCode::Char('G'), n, false, &mut p), Msg::Ignored);
    }

    #[test]
    fn g_prefix_resolves_gg_and_gd() {
        let n = KeyModifiers::NONE;
        let mut p = None;
        assert_eq!(dk(KeyCode::Char('g'), n, true, &mut p), Msg::Ignored);
        assert_eq!(p, Some('g'));
        assert_eq!(dk(KeyCode::Char('g'), n, true, &mut p), Msg::Top);
        assert_eq!(p, None);
        assert_eq!(dk(KeyCode::Char('g'), n, true, &mut p), Msg::Ignored);
        assert_eq!(dk(KeyCode::Char('d'), n, true, &mut p), Msg::Jump);
        assert_eq!(p, None);
    }

    #[test]
    fn g_prefix_cancels_on_other_key() {
        let n = KeyModifiers::NONE;
        let mut p = Some('g');
        let m = dk(KeyCode::Char('k'), n, true, &mut p);
        assert_eq!(m, Msg::Up);
        assert_eq!(p, None);
    }

    #[test]
    fn plain_h_l_and_side_arrows_are_ignored() {
        let n = KeyModifiers::NONE;
        let mut p = None;
        assert_eq!(dk(KeyCode::Char('h'), n, true, &mut p), Msg::Ignored);
        assert_eq!(dk(KeyCode::Char('l'), n, true, &mut p), Msg::Ignored);
        assert_eq!(dk(KeyCode::Left, n, true, &mut p), Msg::Ignored);
        assert_eq!(dk(KeyCode::Right, n, true, &mut p), Msg::Ignored);
    }
}
