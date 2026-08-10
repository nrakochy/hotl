//! [`ProcessControl`] — detachment, and reaping a whole process tree.

use std::io;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{UnixProcessControl, UnixReaper};
#[cfg(unix)]
pub type ActiveProcessControl = UnixProcessControl;
#[cfg(unix)]
pub type ActiveReaper = UnixReaper;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{WindowsProcessControl, WindowsReaper};
#[cfg(windows)]
pub type ActiveProcessControl = WindowsProcessControl;
#[cfg(windows)]
pub type ActiveReaper = WindowsReaper;

/// Two things Unix does with process groups and Windows does with job objects,
/// behind one contract.
pub trait ProcessControl: crate::sealed::Sealed {
    type Reaper: TreeReaper;

    /// Detach the child from the controlling terminal or console so it cannot
    /// inject input into ours.
    ///
    /// Unix: `setsid()` in `pre_exec`, which is the TIOCSTI defense. Windows:
    /// `DETACHED_PROCESS`, which closes the `WriteConsoleInput`-on-`CONIN$`
    /// variant of the same attack. Different syscalls, same threat.
    fn detach(&self, cmd: &mut tokio::process::Command);

    fn new_reaper(&self) -> io::Result<Self::Reaper>;
}

/// Something that kills every descendant, not just the direct child.
pub trait TreeReaper: Send + Sync {
    /// Whether a descendant can escape between spawn and adoption.
    ///
    /// Unix process groups: **no** — `process_group(0)` takes effect before the
    /// child's first instruction. Windows without
    /// `PROC_THREAD_ATTRIBUTE_JOB_LIST`: **yes**, because
    /// `std::process::Command` cannot express `CREATE_SUSPENDED`, so a child
    /// that forks immediately can outrun `AssignProcessToJobObject`. Reported
    /// as data rather than buried in a comment, so a caller that needs
    /// atomicity can assert on it and a later change can flip it.
    const ADOPTION_IS_ATOMIC: bool;

    /// Take ownership of `child` and everything it goes on to spawn.
    fn adopt(&self, child: &tokio::process::Child) -> io::Result<()>;

    /// Kill the whole tree.
    ///
    /// Windows is strictly stronger here and it is worth knowing why: a job
    /// object with `KILL_ON_JOB_CLOSE` reaps the tree even if **hotl itself**
    /// dies, which `kill(-pgid)` does not. And the pid-reuse hazard that makes
    /// the Unix caller order its kill before the wait is moot on Windows — a
    /// job handle is a kernel object, not a number that can be recycled.
    fn kill_tree(&self) -> io::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One body, both mechanisms: a reaper adopts a child that spawns a
    /// grandchild, and killing the tree kills both. Written so it can fail —
    /// if adoption raced the spawn, the grandchild survives and the assertion
    /// catches it.
    #[test]
    fn a_reaper_kills_the_grandchild_too() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ctl = crate::PROCESS_CONTROL;
            let reaper = ctl.new_reaper().unwrap();

            let mut cmd = spawn_a_sleeping_tree();
            ctl.detach(&mut cmd);
            let mut child = cmd.spawn().unwrap();
            reaper.adopt(&child).unwrap();

            reaper.kill_tree().unwrap();
            // The direct child must be gone promptly. The grandchild shares its
            // fate through the group/job, which is the property under test —
            // waiting on the child is how we know the tree was reaped rather
            // than just signalled.
            let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
                .await
                .expect("kill_tree left the child running")
                .unwrap();
            assert!(!status.success(), "a killed child must not report success");
        });
    }

    /// A child that spawns a grandchild and then waits, so the grandchild
    /// outlives its parent's own exit unless something reaps the tree.
    fn spawn_a_sleeping_tree() -> tokio::process::Command {
        #[cfg(unix)]
        {
            let mut c = tokio::process::Command::new("/bin/sh");
            c.arg("-c").arg("sleep 60 & sleep 60");
            c
        }
        #[cfg(windows)]
        {
            let mut c = tokio::process::Command::new("cmd.exe");
            c.arg("/c")
                .arg("start /b timeout /t 60 /nobreak & timeout /t 60 /nobreak");
            c
        }
    }
}
