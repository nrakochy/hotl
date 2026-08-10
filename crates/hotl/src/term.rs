//! Terminal restore that survives what `Drop` does not.
//!
//! Both TUIs (`hotl` and `hotl watch`) take the screen with raw mode inside
//! the alternate buffer and hand it back in a guard's `Drop`. A process killed
//! by a signal never runs `Drop`, so the shell inherits a terminal still in raw
//! mode, still on the alternate screen — no echo, no prompt, no cursor, and a
//! second Ctrl-C needed before the terminal is usable again.
//!
//! Ctrl-C normally reaches the TUI as a key, because raw mode holds `ISIG`
//! off. The wedge shows up whenever something puts sane modes back while the
//! TUI still owns the screen — a child sharing the controlling terminal, the
//! `$EDITOR` suspension, the startup window before `enter()` — because the
//! next Ctrl-C is then a real SIGINT. SIGTERM and SIGHUP (closing the window)
//! wedge it the same way with no Ctrl-C at all.
//!
//! So the restore lives here rather than only in the guard: armed when a guard
//! takes the screen, run by whichever teardown arrives first — `Drop`, the
//! panic hook, or the signal handler. Guards keep using crossterm on the
//! normal paths so its internal mode state stays honest; the handler path
//! touches only async-signal-safe calls (`tcsetattr`, `write`, `_exit`)
//! against a `termios` captured before raw mode, so it never waits on
//! crossterm's mutex.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use hotl_platform::ConsoleControl;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::{CrosstermBackend, Terminal};

/// Disable mouse reporting (button, any-motion, SGR encoding) and bracketed
/// paste, leave the alternate screen, then show the cursor — in that order, so
/// nothing we turned on outlives the process. A shell that inherits mouse
/// reporting emits escape sequences on every mouse move, which is strictly
/// worse than the raw-mode wedge this module exists to prevent.
/// INVARIANT: undoes every mode `TerminalGuard::enter` sets. Enforced by
/// `restore_bytes_disable_mouse_and_bracketed_paste`.
const RESTORE: &[u8] =
    b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l\x1b[?25h";

/// Owns raw mode, the alternate screen, and — when asked — mouse reporting and
/// bracketed paste, restoring all of it on drop. An early error during setup,
/// a normal exit, or a panic inside a run loop all leave the shell usable.
///
/// One guard for both TUIs (`hotl` and `hotl watch`). They had separate,
/// already-drifted copies (§7); this lives here because [`RESTORE`] — the
/// async-signal-safe byte string the signal handler writes — must disable
/// exactly the set `enter` enables.
pub(crate) struct TerminalGuard {
    pub(crate) terminal: Terminal<CrosstermBackend<Stdout>>,
    /// Mouse capture is on. Recorded so `suspend`/`resume`/`Drop` stay
    /// symmetric with whatever `enter` decided.
    mouse: bool,
}

impl TerminalGuard {
    /// Take the screen. `mouse` requests wheel and drag reporting — it costs
    /// the terminal's own drag-select, which the console gives back as
    /// `[behavior] copy_on_select`. Gated on `[behavior] mouse` / `HOTL_MOUSE`
    /// there; the watch dashboard (nothing to scroll) passes `false`.
    pub(crate) fn enter(mouse: bool) -> io::Result<Self> {
        capture();
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // If entering the alt screen fails, undo raw mode before propagating.
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        if mouse {
            let _ = execute!(stdout, EnableMouseCapture);
        }
        let _ = execute!(stdout, EnableBracketedPaste);
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => {
                arm();
                Ok(TerminalGuard { terminal, mouse })
            }
            Err(e) => {
                let _ = execute!(
                    io::stdout(),
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    LeaveAlternateScreen
                );
                let _ = disable_raw_mode();
                Err(e)
            }
        }
    }

    /// Turn mouse reporting on or off while the screen is held (`/reload`
    /// picking up a changed `[behavior] mouse`). The field moves with it, so
    /// `suspend`/`resume`/`Drop` stay symmetric with the *current* setting
    /// rather than whatever `enter` was told at startup.
    pub(crate) fn set_mouse(&mut self, on: bool) {
        if on == self.mouse {
            return;
        }
        self.mouse = on;
        let _ = if on {
            execute!(self.terminal.backend_mut(), EnableMouseCapture)
        } else {
            execute!(self.terminal.backend_mut(), DisableMouseCapture)
        };
    }

    /// Hand the real screen to `$EDITOR` — every mode we set goes with it.
    pub(crate) fn suspend(&mut self) {
        self.release();
        disarm();
    }

    /// …and take it back.
    pub(crate) fn resume(&mut self) {
        let _ = enable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), EnterAlternateScreen);
        if self.mouse {
            let _ = execute!(self.terminal.backend_mut(), EnableMouseCapture);
        }
        let _ = execute!(self.terminal.backend_mut(), EnableBracketedPaste);
        let _ = self.terminal.clear();
        arm();
    }

    /// Undo every mode `enter` set, in `RESTORE`'s order. Best-effort:
    /// nothing is actionable if these fail on the way out.
    fn release(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), DisableBracketedPaste);
        if self.mouse {
            let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        }
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.release();
        let _ = self.terminal.show_cursor();
        disarm();
    }
}

/// Set while a guard owns the screen. The teardown that clears it does the
/// restore; every other one is a no-op.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The terminal's modes from before raw mode.
///
/// A `OnceLock` rather than the leaked `AtomicPtr` this used to be: the modes
/// are now an opaque `Copy` type owned by the caller, so there is nothing to
/// leak and the handler can still read them without allocating or locking.
static ORIGINAL: OnceLock<<hotl_platform::ActiveConsoleControl as ConsoleControl>::Saved> =
    OnceLock::new();

/// Remember the cooked modes. Call before the first `enable_raw_mode`; later
/// calls are ignored so an `$EDITOR` round-trip can't save raw modes as the
/// thing to restore to.
pub(crate) fn capture() {
    if ORIGINAL.get().is_some() {
        return;
    }
    if let Ok(modes) = hotl_platform::CONSOLE.capture() {
        let _ = ORIGINAL.set(modes);
    }
}

/// The guard now owns the screen — signals and panics must clean up after it.
pub(crate) fn arm() {
    ARMED.store(true, Ordering::SeqCst);
}

/// The guard has restored the screen itself (normal `Drop`, or the suspend
/// that hands the terminal to `$EDITOR`).
pub(crate) fn disarm() {
    ARMED.store(false, Ordering::SeqCst);
}

/// Put the terminal back if nobody else has. `false` means it was already
/// restored, which is the common case on the normal exit path.
pub(crate) fn restore() -> bool {
    if !ARMED.swap(false, Ordering::SeqCst) {
        return false;
    }
    reset();
    true
}

/// The whole restore, in calls the handler is allowed to make.
///
/// On Unix that means async-signal-safe and nothing else — no allocation, no
/// locks, which is why this reaches for `ConsoleControl` rather than crossterm.
/// Windows runs the handler on its own thread and would tolerate more, but one
/// body for both is what keeps the two from drifting.
fn reset() {
    if let Some(original) = ORIGINAL.get() {
        let _ = hotl_platform::CONSOLE.restore(original);
    }
    hotl_platform::CONSOLE.write_raw(RESTORE);
}

/// Catch the interrupt-class events that would otherwise skip every
/// destructor. Unix handlers are reset across `exec`, so spawned tools still
/// get the default disposition.
pub(crate) fn trap_signals() {
    let _ = hotl_platform::CONSOLE.trap(|| {
        if ARMED.swap(false, Ordering::SeqCst) {
            reset();
        }
    });
}

/// Restore before the panic message prints, so it lands on a live screen
/// instead of the alternate buffer that is about to be thrown away.
pub(crate) fn restore_on_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These share process-global state, so they run as one test.
    #[test]
    fn only_the_first_teardown_restores() {
        disarm();
        assert!(!restore(), "a disarmed terminal needs no restoring");

        arm();
        assert!(restore(), "the first teardown after arming does the work");
        assert!(!restore(), "the second finds nothing left to do");

        arm();
        disarm();
        assert!(
            !restore(),
            "a guard that restored itself leaves nothing behind"
        );
    }

    /// The signal-path restore must undo everything a guard turns on, not just
    /// the alternate screen — a killed TUI that leaves mouse reporting or
    /// bracketed paste enabled poisons the user's shell worse than raw mode
    /// does.
    #[test]
    fn restore_bytes_disable_mouse_and_bracketed_paste() {
        let s = std::str::from_utf8(RESTORE).unwrap();
        assert!(s.contains("\x1b[?1049l"), "leave alternate screen");
        assert!(s.contains("\x1b[?25h"), "show cursor");
        assert!(s.contains("\x1b[?1006l"), "disable SGR mouse encoding");
        assert!(
            s.contains("\x1b[?1003l"),
            "disable any-motion mouse tracking"
        );
        assert!(
            s.contains("\x1b[?1000l"),
            "disable button-event mouse tracking"
        );
        assert!(s.contains("\x1b[?2004l"), "disable bracketed paste");
    }

    /// §7: two near-identical guards, already drifted — `tui.rs`'s grew
    /// `suspend`/`resume` and mouse + bracketed paste, `watch.rs`'s did not,
    /// so `hotl watch` would not have restored modes a future feature turns
    /// on. One guard, in the module that already owns the signal-path restore
    /// those modes must match.
    #[test]
    fn one_guard_serves_both_tuis() {
        let src = concat!(include_str!("tui.rs"), include_str!("watch.rs"));
        assert!(
            !src.contains(concat!("struct Terminal", "Guard")),
            "the guard lives in term.rs"
        );
    }

    #[test]
    fn signals_report_the_shell_convention() {
        // 130 = 128 + SIGINT. Asserted on every platform: it is a POSIX *shell*
        // convention with no Windows meaning, kept there anyway because scripts
        // and CI check for it, and a test is what stops that from looking like
        // an accident later.
        assert_eq!(hotl_platform::CONSOLE.interrupt_exit_code(), 130);
        #[cfg(unix)]
        {
            assert_eq!(128 + libc::SIGTERM, 143);
            assert_eq!(128 + libc::SIGHUP, 129);
        }
    }

    /// The regression: a trapped signal must leave through the handler (an
    /// ordinary exit) instead of killing the process with every destructor,
    /// terminal restore included, unrun. Forked so the assertion survives it.
    ///
    /// Unix-only because `fork` is. The Windows twin — re-exec `current_exe()`
    /// with a marker env var under `CREATE_NEW_PROCESS_GROUP`, then
    /// `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT)` at it — is owed and is
    /// tracked in plan 0027; it needs a console, which no CI-safe fake
    /// provides, so it has to be written against a real one rather than
    /// guessed at here.
    #[test]
    #[cfg(unix)]
    fn a_trapped_signal_exits_instead_of_killing_us() {
        const TRAPPED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
        let exit_code = |signal: libc::c_int| 128 + signal;
        for signal in TRAPPED {
            let child = unsafe { libc::fork() };
            assert!(child >= 0, "fork failed");
            if child == 0 {
                trap_signals();
                arm();
                unsafe { libc::raise(signal) };
                // Only reached if the handler never ran.
                unsafe { libc::_exit(1) };
            }
            let mut status = 0;
            assert!(unsafe { libc::waitpid(child, &mut status, 0) } > 0);
            assert!(
                libc::WIFEXITED(status),
                "signal {signal} killed the process instead of being handled"
            );
            assert_eq!(libc::WEXITSTATUS(status), exit_code(signal));
        }
    }
}
