//! A job object with kill-on-close, plus `DETACHED_PROCESS`.

use super::{ProcessControl, TreeReaper};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsProcessControl;

impl WindowsProcessControl {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for WindowsProcessControl {}

impl ProcessControl for WindowsProcessControl {
    type Reaper = WindowsReaper;

    fn detach(&self, cmd: &mut tokio::process::Command) {
        // `DETACHED_PROCESS` gives the child no console at all, so it cannot
        // call `WriteConsoleInput` on our `CONIN$` — the Windows shape of the
        // TIOCSTI attack `setsid` closes on Unix. `CREATE_NEW_PROCESS_GROUP`
        // keeps a console Ctrl-C from reaching it as a side effect of reaching
        // us.
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    fn new_reaper(&self) -> io::Result<Self::Reaper> {
        // SAFETY: an anonymous job object with default security.
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh kernel handle owned by nothing else.
        let job = unsafe { OwnedHandle::from_raw_handle(job as _) };

        // SAFETY: zeroed is a valid instance of this plain struct.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // `KILL_ON_JOB_CLOSE` is what makes this stronger than `kill(-pgid)`:
        // the tree dies even if hotl itself dies, because the last handle
        // closing is what triggers it.
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        // SAFETY: a job handle we own and a live, correctly-sized info struct.
        let ok = unsafe {
            SetInformationJobObject(
                job.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(WindowsReaper(job))
    }
}

pub struct WindowsReaper(OwnedHandle);

impl TreeReaper for WindowsReaper {
    /// **False, and this is the honest answer rather than the convenient one.**
    ///
    /// `std::process::Command` cannot express `CREATE_SUSPENDED`, so there is a
    /// window between spawn and `AssignProcessToJobObject` in which a child
    /// that immediately spawns a grandchild leaves it outside the job. Closing
    /// it needs `PROC_THREAD_ATTRIBUTE_JOB_LIST`, which assigns the job
    /// *before the initial thread runs* — and that needs a launcher that calls
    /// `CreateProcess` itself. Until that ships, this reports the gap instead
    /// of hiding it.
    const ADOPTION_IS_ATOMIC: bool = false;

    fn adopt(&self, child: &tokio::process::Child) -> io::Result<()> {
        let Some(pid) = child.id() else {
            return Err(io::Error::other("the child has already been reaped"));
        };
        // Re-open by pid rather than using tokio's handle: tokio owns that one
        // and closing it out from under the `Child` would break the wait.
        // SAFETY: plain OpenProcess with the two rights the job needs.
        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a handle we just opened and own.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
        // SAFETY: both are handles we own.
        let ok = unsafe {
            AssignProcessToJobObject(
                self.0.as_raw_handle() as HANDLE,
                handle.as_raw_handle() as HANDLE,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn kill_tree(&self) -> io::Result<()> {
        // Every process in the job, at any depth. No pid-reuse hazard to reason
        // about: a job is a kernel object, not a recyclable number.
        // SAFETY: a job handle we own.
        if unsafe { TerminateJobObject(self.0.as_raw_handle() as HANDLE, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
