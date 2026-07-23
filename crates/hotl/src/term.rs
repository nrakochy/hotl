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

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// Leave the alternate screen, then show the cursor.
const RESTORE: &[u8] = b"\x1b[?1049l\x1b[?25h";

/// The signals that kill a foreground TUI outright. SIGQUIT is left alone:
/// it is the deliberate "core-dump this" escape hatch.
const TRAPPED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Set while a guard owns the screen. The teardown that clears it does the
/// restore; every other one is a no-op.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The terminal's modes from before raw mode, leaked once so the signal
/// handler can read them without allocating or locking.
static ORIGINAL: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());

/// Remember the cooked modes. Call before the first `enable_raw_mode`; later
/// calls are ignored so an `$EDITOR` round-trip can't save raw modes as the
/// thing to restore to.
pub(crate) fn capture() {
    if !ORIGINAL.load(Ordering::SeqCst).is_null() {
        return;
    }
    let mut modes: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut modes) } == 0 {
        ORIGINAL.store(Box::into_raw(Box::new(modes)), Ordering::SeqCst);
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

/// The whole restore in async-signal-safe calls: no allocation, no locks.
fn reset() {
    let original = ORIGINAL.load(Ordering::SeqCst);
    if !original.is_null() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original) };
    }
    unsafe { libc::write(libc::STDOUT_FILENO, RESTORE.as_ptr().cast(), RESTORE.len()) };
}

/// What the shell reports for "killed by this signal".
fn exit_code(signal: libc::c_int) -> libc::c_int {
    128 + signal
}

extern "C" fn on_signal(signal: libc::c_int) {
    if ARMED.swap(false, Ordering::SeqCst) {
        reset();
    }
    unsafe { libc::_exit(exit_code(signal)) };
}

/// Catch the signals that would otherwise skip every destructor. Handlers are
/// reset across `exec`, so spawned tools still get the default disposition.
pub(crate) fn trap_signals() {
    for signal in TRAPPED {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = libc::SA_RESTART;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
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

    #[test]
    fn signals_report_the_shell_convention() {
        assert_eq!(exit_code(libc::SIGINT), 130);
        assert_eq!(exit_code(libc::SIGTERM), 143);
        assert_eq!(exit_code(libc::SIGHUP), 129);
    }

    /// The regression: a trapped signal must leave through the handler (an
    /// ordinary exit) instead of killing the process with every destructor,
    /// terminal restore included, unrun. Forked so the assertion survives it.
    #[test]
    fn a_trapped_signal_exits_instead_of_killing_us() {
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
