//! `setsid` + a process group, reaped with `kill(-pgid)`.

use super::{ProcessControl, TreeReaper};
use std::io;
use std::sync::atomic::{AtomicI32, Ordering};

#[derive(Debug, Clone, Copy, Default)]
pub struct UnixProcessControl;

impl UnixProcessControl {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for UnixProcessControl {}

impl ProcessControl for UnixProcessControl {
    type Reaper = UnixReaper;

    fn detach(&self, cmd: &mut tokio::process::Command) {
        // `setsid` alone, and deliberately **not** alongside
        // `Command::process_group(0)`: setsid already creates both a new
        // session and a new process group whose pgid equals the pid, and it
        // fails with EPERM if the caller is *already* a group leader — which
        // `process_group(0)` would have just made it. Asking for both is how
        // you get a spawn that cannot start.
        //
        // The session is what detaches the controlling terminal and closes the
        // TIOCSTI class; the pgid is what makes the reaper's negated kill reach
        // the whole tree.
        // SAFETY: `setsid` is async-signal-safe and touches no shared state; it
        // is the only work done between fork and exec here.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    fn new_reaper(&self) -> io::Result<Self::Reaper> {
        Ok(UnixReaper(AtomicI32::new(0)))
    }
}

/// The child's pgid, which `detach` arranged to equal its pid.
pub struct UnixReaper(AtomicI32);

impl TreeReaper for UnixReaper {
    const ADOPTION_IS_ATOMIC: bool = true;

    fn adopt(&self, child: &tokio::process::Child) -> io::Result<()> {
        // Nothing to do at the kernel: `process_group(0)` already took effect
        // before the child ran its first instruction, which is exactly why
        // `ADOPTION_IS_ATOMIC` is true here. This only records the number.
        let Some(pid) = child.id() else {
            return Err(io::Error::other("the child has already been reaped"));
        };
        self.0.store(pid as i32, Ordering::SeqCst);
        Ok(())
    }

    /// INVARIANT: only ever called while the `Child` is still owned and
    /// un-reaped, so the pid is either live or a zombie — reserved either way,
    /// and never reusable by another process. Killing after a wait would be a
    /// pid-reuse bug: the number could by then name someone else's process, and
    /// the negation someone else's *group*.
    fn kill_tree(&self) -> io::Result<()> {
        let pid = self.0.load(Ordering::SeqCst);
        if pid == 0 {
            return Ok(()); // nothing adopted
        }
        // SAFETY: plain kill(2); a negative pid targets the process group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        Ok(())
    }
}
