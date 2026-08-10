//! `termios` + `sigaction`, in async-signal-safe calls only.

use super::{ConsoleControl, HandlerContract};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, Default)]
pub struct UnixConsoleControl;

impl UnixConsoleControl {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for UnixConsoleControl {}

/// The cooked modes, `Copy` so the caller can hold them in a `OnceLock` rather
/// than leaking a `Box` for the handler to read.
#[derive(Clone, Copy)]
pub struct UnixModes(libc::termios);

// SAFETY: `termios` is a plain repr(C) struct of integers with no interior
// pointers; sharing a copy across threads is sound.
unsafe impl Send for UnixModes {}
unsafe impl Sync for UnixModes {}

/// The callback, as a plain function pointer so the handler allocates nothing.
/// `usize` because `AtomicPtr` would need a concrete pointee type.
static ON_INTERRUPT: AtomicUsize = AtomicUsize::new(0);

/// The signals that kill a foreground TUI outright. `SIGQUIT` is left alone: it
/// is the deliberate "core-dump this" escape hatch.
const TRAPPED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

extern "C" fn on_signal(signal: libc::c_int) {
    let f = ON_INTERRUPT.load(Ordering::SeqCst);
    if f != 0 {
        // SAFETY: only ever stored from `trap`, and only ever a `fn()`.
        let f: fn() = unsafe { std::mem::transmute::<usize, fn()>(f) };
        f();
    }
    // SAFETY: `_exit` is async-signal-safe; `exit` is not.
    unsafe { libc::_exit(128 + signal) };
}

impl ConsoleControl for UnixConsoleControl {
    type Saved = UnixModes;

    const HANDLER_CONTRACT: HandlerContract = HandlerContract::AsyncSignalSafe;

    fn capture(&self) -> io::Result<Self::Saved> {
        // SAFETY: `termios` is a plain repr(C) struct; all-zero is valid.
        let mut modes: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a live out-param and a standard fd.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut modes) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(UnixModes(modes))
    }

    fn restore(&self, saved: &Self::Saved) -> io::Result<()> {
        // SAFETY: `tcsetattr` is async-signal-safe, which is what lets this be
        // called from the handler.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved.0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn write_raw(&self, bytes: &[u8]) {
        // SAFETY: `write(2)` is async-signal-safe. A short or failed write has
        // nothing to report from a signal context.
        unsafe {
            libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len());
        }
    }

    fn trap(&self, on_interrupt: fn()) -> io::Result<()> {
        ON_INTERRUPT.store(on_interrupt as usize, Ordering::SeqCst);
        for signal in TRAPPED {
            // SAFETY: installing a handler that touches only async-signal-safe
            // calls, per `HANDLER_CONTRACT`. Handlers are reset across `exec`,
            // so spawned tools still get the default disposition.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
                libc::sigemptyset(&mut action.sa_mask);
                action.sa_flags = libc::SA_RESTART;
                if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }

    fn interrupt_exit_code(&self) -> i32 {
        128 + libc::SIGINT
    }
}
