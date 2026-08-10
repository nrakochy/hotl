//! Console modes + `SetConsoleCtrlHandler`.

use super::{ConsoleControl, HandlerContract};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, SetConsoleCtrlHandler, SetConsoleMode, WriteConsoleA,
    CONSOLE_MODE, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsConsoleControl;

impl WindowsConsoleControl {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for WindowsConsoleControl {}

/// **Both** modes, not just input.
///
/// The output mode carries `ENABLE_VIRTUAL_TERMINAL_PROCESSING`, which is what
/// makes the escape-sequence half of a restore mean anything. Saving only the
/// input mode is the easy version of this and it leaves the terminal wrong in a
/// way that is hard to attribute later.
#[derive(Clone, Copy)]
pub struct WindowsModes {
    input: CONSOLE_MODE,
    output: CONSOLE_MODE,
}

static ON_INTERRUPT: AtomicUsize = AtomicUsize::new(0);

/// `CTRL_CLOSE_EVENT` gives the handler roughly five seconds before the process
/// is killed regardless.
const CLOSE_EVENT_BUDGET_MS: u32 = 5000;

unsafe extern "system" fn on_ctrl(event: u32) -> BOOL {
    if !matches!(
        event,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        return 0; // not ours — let the next handler decide
    }
    let f = ON_INTERRUPT.load(Ordering::SeqCst);
    if f != 0 {
        // SAFETY: only ever stored from `trap`, and only ever a `fn()`. Unlike
        // the Unix twin this runs on its own thread, so it may allocate and
        // lock — see `HANDLER_CONTRACT`.
        let f: fn() = unsafe { std::mem::transmute::<usize, fn()>(f) };
        f();
    }
    std::process::exit(130);
}

fn std_handle(which: u32) -> io::Result<HANDLE> {
    // SAFETY: a documented, infallible-for-valid-input accessor.
    let h = unsafe { GetStdHandle(which) };
    if h.is_null() || h as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(h)
}

impl ConsoleControl for WindowsConsoleControl {
    type Saved = WindowsModes;

    const HANDLER_CONTRACT: HandlerContract = HandlerContract::SeparateThreadWithBudget {
        millis: CLOSE_EVENT_BUDGET_MS,
    };

    fn capture(&self) -> io::Result<Self::Saved> {
        let (i, o) = (
            std_handle(STD_INPUT_HANDLE)?,
            std_handle(STD_OUTPUT_HANDLE)?,
        );
        let mut input: CONSOLE_MODE = 0;
        let mut output: CONSOLE_MODE = 0;
        // SAFETY: handles we just validated, and live out-params.
        if unsafe { GetConsoleMode(i, &mut input) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { GetConsoleMode(o, &mut output) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(WindowsModes { input, output })
    }

    fn restore(&self, saved: &Self::Saved) -> io::Result<()> {
        let (i, o) = (
            std_handle(STD_INPUT_HANDLE)?,
            std_handle(STD_OUTPUT_HANDLE)?,
        );
        // SAFETY: handles we just validated.
        if unsafe { SetConsoleMode(i, saved.input) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above. The output mode matters as much as the input one.
        if unsafe { SetConsoleMode(o, saved.output) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn write_raw(&self, bytes: &[u8]) {
        let Ok(h) = std_handle(STD_OUTPUT_HANDLE) else {
            return;
        };
        let mut written = 0u32;
        // SAFETY: a validated handle and a live slice. A failed write has
        // nothing useful to report on a teardown path.
        unsafe {
            WriteConsoleA(
                h,
                bytes.as_ptr().cast(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null(),
            );
        }
    }

    fn trap(&self, on_interrupt: fn()) -> io::Result<()> {
        ON_INTERRUPT.store(on_interrupt as usize, Ordering::SeqCst);
        // SAFETY: installing a handler that respects the budget above.
        if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn interrupt_exit_code(&self) -> i32 {
        // 128 + SIGINT. A POSIX shell convention with no Windows meaning, kept
        // because scripts and CI check for it — see the trait doc.
        130
    }
}
