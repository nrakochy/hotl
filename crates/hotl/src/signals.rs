//! One interrupt stream for a surface's lifetime.
//!
//! Deliberately **not** `tokio::signal::ctrl_c()`: that is a one-shot future,
//! and re-arming it on every `select!` iteration is the thing this type exists
//! to avoid. A surface holds one `Interrupt` as a field and polls it, so no
//! interrupt is lost between iterations and no registration is repeated.

/// A stream of interrupts — SIGINT on Unix, Ctrl-C on Windows.
pub(crate) struct Interrupt {
    #[cfg(unix)]
    inner: tokio::signal::unix::Signal,
    #[cfg(windows)]
    inner: tokio::signal::windows::CtrlC,
}

impl Interrupt {
    /// Registers the handler. Fails only if the OS refuses, which on both
    /// platforms means the process cannot be interrupted at all — a surface
    /// treats that as fatal rather than running deaf.
    pub(crate) fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                inner: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                inner: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    /// Resolves on the next interrupt. `None` means the stream ended, which
    /// only happens at shutdown.
    pub(crate) async fn recv(&mut self) -> Option<()> {
        self.inner.recv().await
    }
}
