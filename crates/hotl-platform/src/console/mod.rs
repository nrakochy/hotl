//! [`ConsoleControl`] — save/restore terminal state, and trap the
//! interrupt-class events that would otherwise skip every destructor.

use std::io;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{UnixConsoleControl, UnixModes};
#[cfg(unix)]
pub type ActiveConsoleControl = UnixConsoleControl;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{WindowsConsoleControl, WindowsModes};
#[cfg(windows)]
pub type ActiveConsoleControl = WindowsConsoleControl;

/// What an implementor's interrupt handler may do.
///
/// This is part of the contract, not trivia: a handler that is legal on one OS
/// is undefined behavior on the other, and a caller cannot write one correctly
/// without knowing which it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerContract {
    /// The handler runs on an arbitrary thread inside a signal context. **No
    /// allocation, no locks, no non-reentrant libc.** Anything else is UB.
    AsyncSignalSafe,
    /// `SetConsoleCtrlHandler` runs the handler on a thread of its own, and the
    /// process is killed if it has not returned within roughly this budget for
    /// `CTRL_CLOSE_EVENT`. It **may** allocate and lock.
    SeparateThreadWithBudget { millis: u32 },
}

/// Save/restore terminal state and trap the interrupt-class events.
pub trait ConsoleControl: crate::sealed::Sealed {
    /// The saved modes, held by the *caller* — which is what lets the caller
    /// keep them in a `OnceLock` rather than a leaked `AtomicPtr`.
    type Saved: Send + Sync + Copy + 'static;

    const HANDLER_CONTRACT: HandlerContract;

    /// Capture the modes as they are *now*. Call before anything enters raw
    /// mode: a later call would save raw modes as the thing to restore to.
    fn capture(&self) -> io::Result<Self::Saved>;

    /// Put the modes back. Must be callable from the interrupt handler, so on
    /// Unix this is `tcsetattr` and nothing else.
    fn restore(&self, saved: &Self::Saved) -> io::Result<()>;

    /// Write bytes straight to the terminal, bypassing any buffering — the
    /// escape-sequence half of a restore. Async-signal-safe on Unix.
    fn write_raw(&self, bytes: &[u8]);

    /// Run `on_interrupt` for each interrupt-class event, then exit.
    ///
    /// `on_interrupt` must respect [`HANDLER_CONTRACT`](ConsoleControl::HANDLER_CONTRACT).
    /// A plain `fn` rather than a closure so there is nothing to allocate or
    /// capture on the Unix path.
    fn trap(&self, on_interrupt: fn()) -> io::Result<()>;

    /// The process exit code for "killed by an interrupt".
    ///
    /// `128 + signal` is a POSIX **shell** convention with no Windows meaning.
    /// Windows returns 130 for Ctrl-C anyway, because scripts and CI check for
    /// it — a deliberate borrowing, flagged here rather than left to look like
    /// an accident.
    fn interrupt_exit_code(&self) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capture and restore round-trip, on whatever this build selected.
    ///
    /// Deliberately does *not* assert a terminal exists: under a test harness
    /// stdin may or may not be one, and a test whose verdict depends on how it
    /// was invoked is worse than no test. What is asserted is the rule that
    /// holds either way — an adapter that cannot capture reports the OS's own
    /// error rather than inventing one.
    ///
    /// `raw_os_error()` is the discriminator, not `kind()`: a real errno always
    /// carries one, and a synthesized `Error::new(Unsupported, "…")` never
    /// does. Testing `kind()` was the first version of this and it was wrong —
    /// errno-to-kind mapping is a std implementation detail that can land on
    /// `Unsupported` legitimately, which made the test fail whenever stdin was
    /// not a tty.
    #[test]
    fn a_console_that_cannot_be_captured_reports_the_os_error_rather_than_inventing_one() {
        let ctl = crate::CONSOLE;
        match ctl.capture() {
            Ok(saved) => ctl.restore(&saved).expect("captured modes must restore"),
            Err(e) => assert!(
                e.raw_os_error().is_some(),
                "the failure must carry a real errno, not a synthesized one: {e:?}"
            ),
        }
    }

    #[test]
    fn the_interrupt_exit_code_is_the_shell_convention() {
        // 130 = 128 + SIGINT, on both platforms and for the same reason: it is
        // what scripts and CI check for.
        assert_eq!(crate::CONSOLE.interrupt_exit_code(), 130);
    }
}
