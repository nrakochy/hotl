use std::collections::VecDeque;
use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;

use crate::term::TerminalGuard;
use watch_listener::Listener;
use watch_tmux::TmuxSurface;
use watch_tui::{decode_key, update, AppState, Cmd, Msg};
use watch_types::{AgentObservation, HotlConfig, Status};

// Spinner advances this often, but only while an agent is working.
const ANIM_INTERVAL: Duration = Duration::from_millis(125);

/// Entry point for `hotl watch` — the dashboard, exactly as it shipped pre-merge.
pub fn watch_main() -> io::Result<()> {
    if !watch_tmux::tmux_available() {
        eprintln!("hotl watch: tmux not found on PATH");
        std::process::exit(1);
    }
    let (cfg, config_warning) = HotlConfig::load_with_warning();
    let listener = Listener::new(vec![Box::new(TmuxSurface::new(&cfg.settings.agents))]);

    // `catch_unwind` below covers panics; signals skip every destructor, so
    // they get their own restore.
    crate::term::trap_signals();
    let mut guard = TerminalGuard::enter(false)?;
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
    // Resolved once — the render loop reuses the converted palette per frame.
    let palette = hotl_theme::Palette::from(&cfg.settings.theme.resolve().0);
    let mut state = AppState::new(cfg.settings.ping_on_blocked, cfg.settings.vim_mode);
    if let Some(warn) = config_warning {
        state.status = warn;
    }
    let mut pending: Option<char> = None;
    let mut last_tick = Instant::now()
        .checked_sub(tick)
        .unwrap_or_else(Instant::now);
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
            let timeout = wait_for(tick, last_tick.elapsed(), working, last_anim.elapsed());
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
        terminal.draw(|f| watch_tui::view(&state, &palette, f))?;
    }
    Ok(())
}

/// How long to block on a key: never past the next scan, and — while
/// something is working — never past the next animation frame either. Pure,
/// so the saturating arithmetic that keeps an overdue deadline from
/// underflowing is testable without a clock.
fn wait_for(tick: Duration, since_tick: Duration, working: bool, since_anim: Duration) -> Duration {
    let scan_left = tick.saturating_sub(since_tick);
    if working {
        scan_left.min(ANIM_INTERVAL.saturating_sub(since_anim))
    } else {
        scan_left
    }
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
                watch_types::LocationHandle::Tmux { pane_id, .. } => pane_id.clone(),
            };
            Some(Msg::Jumped(
                listener.focus(&obs).map(|_| id).map_err(|e| e.to_string()),
            ))
        }
        Cmd::SelectPane(dir) => match watch_tmux::select_pane(dir) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_poll_timeout_never_outruns_the_animation() {
        let tick = Duration::from_millis(1000);
        // Idle: wait out the whole scan interval.
        assert_eq!(wait_for(tick, Duration::ZERO, false, Duration::ZERO), tick);
        // Working: never sleep past the next animation frame.
        assert_eq!(
            wait_for(tick, Duration::ZERO, true, Duration::ZERO),
            ANIM_INTERVAL
        );
        // Overdue on both: zero, not an underflow.
        assert_eq!(
            wait_for(tick, tick * 2, true, ANIM_INTERVAL * 2),
            Duration::ZERO
        );
    }
}
