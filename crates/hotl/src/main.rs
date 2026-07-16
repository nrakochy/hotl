use std::collections::VecDeque;
use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;

use listener::Listener;
use tmux::TmuxSurface;
use tui::{decode_key, update, AppState, Cmd, Msg};
use types::{AgentObservation, HotlConfig, Status};

// Spinner advances this often, but only while an agent is working.
const ANIM_INTERVAL: Duration = Duration::from_millis(125);

/// Owns the terminal's raw-mode / alternate-screen state and restores it on
/// drop — so an early `?` during setup, a normal exit, or a panic inside the
/// run loop all leave the user's shell usable instead of stuck in raw mode.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // If entering the alt screen fails, undo raw mode before propagating.
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(t) => t,
            Err(e) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(e);
            }
        };
        Ok(TerminalGuard { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore; nothing actionable if these fail on the way out.
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn main() -> io::Result<()> {
    if !tmux::tmux_available() {
        eprintln!("hotl: tmux not found on PATH");
        std::process::exit(1);
    }
    let (cfg, config_warning) = HotlConfig::load_with_warning();
    let listener = Listener::new(vec![Box::new(TmuxSurface::new(&cfg.settings.agents))]);

    let mut guard = TerminalGuard::enter()?;
    // Catch a panic in the run loop so the guard's Drop restores the terminal
    // first, then re-raise so the panic message renders on the real screen.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut guard.terminal, &listener, &cfg, config_warning)
    }));
    drop(guard);
    match result {
        Ok(r) => r,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    listener: &Listener,
    cfg: &HotlConfig,
    config_warning: Option<String>,
) -> io::Result<()> {
    let tick = Duration::from_millis(cfg.settings.poll_interval_ms_clamped());
    let (theme, _) = cfg.settings.theme.resolve();
    let mut state = AppState::new(cfg.settings.ping_on_blocked, cfg.settings.vim_mode);
    if let Some(warn) = config_warning {
        state.status = warn;
    }
    let mut pending: Option<char> = None;
    let mut last_tick = Instant::now().checked_sub(tick).unwrap_or_else(Instant::now);
    let mut last_anim = Instant::now();

    loop {
        // Advance the spinner only while something is working (redraw-only; no Msg).
        let working = any_working(&state.agents);
        if working && last_anim.elapsed() >= ANIM_INTERVAL {
            state.spinner_tick = state.spinner_tick.wrapping_add(1);
            last_anim = Instant::now();
        }

        let msg = if last_tick.elapsed() >= tick {
            last_tick = Instant::now();
            Some(Msg::Tick)
        } else {
            let scan_left = tick.saturating_sub(last_tick.elapsed());
            let timeout = if working {
                scan_left.min(ANIM_INTERVAL.saturating_sub(last_anim.elapsed()))
            } else {
                scan_left
            };
            match next_key(timeout)? {
                Some((code, mods)) => Some(decode_key(code, mods, state.vim_mode, &mut pending)),
                None => None,
            }
        };

        if let Some(msg) = msg {
            // FIFO: commands run in the order update() produced them, and
            // follow-ups queue behind the current batch.
            let mut queue: VecDeque<Cmd> = update(&mut state, msg).into();
            while let Some(cmd) = queue.pop_front() {
                if let Some(followup) = execute(cmd, listener) {
                    queue.extend(update(&mut state, followup));
                }
            }
        }

        if state.should_quit {
            break;
        }
        terminal.draw(|f| tui::view(&state, &theme, f))?;
    }
    Ok(())
}

fn next_key(timeout: Duration) -> io::Result<Option<(KeyCode, KeyModifiers)>> {
    if event::poll(timeout)? {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                return Ok(Some((key.code, key.modifiers)));
            }
        }
    }
    Ok(None)
}

fn any_working(agents: &[AgentObservation]) -> bool {
    agents.iter().any(|o| o.status == Status::Working)
}

fn execute(cmd: Cmd, listener: &Listener) -> Option<Msg> {
    match cmd {
        Cmd::Scan => Some(Msg::Scanned(
            listener
                .snapshot()
                .map(|snap| (snap.observations, snap.warnings))
                .map_err(|e| e.to_string()),
        )),
        Cmd::Jump(obs) => {
            let id = match &obs.location.handle {
                types::LocationHandle::Tmux { pane_id, .. } => pane_id.clone(),
            };
            Some(Msg::Jumped(listener.focus(&obs).map(|_| id).map_err(|e| e.to_string())))
        }
        Cmd::SelectPane(dir) => match tmux::select_pane(dir) {
            Ok(None) => None,
            Ok(Some(msg)) => Some(Msg::PaneSelectFailed(msg)),
            Err(e) => Some(Msg::PaneSelectFailed(e.to_string())),
        },
        Cmd::Ping => {
            ping();
            None
        }
        Cmd::Quit => None,
    }
}

fn ping() {
    let _ = io::stdout().write_all(b"\x07");
    let _ = io::stdout().flush();
    #[cfg(target_os = "macos")]
    {
        if let Ok(mut child) = std::process::Command::new("afplay")
            .arg("/System/Library/Sounds/Ping.aiff")
            .spawn()
        {
            // Reap on a detached thread so the child never lingers as a zombie
            // and the UI loop isn't blocked waiting for the sound to finish.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}
