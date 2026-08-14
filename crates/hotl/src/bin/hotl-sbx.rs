//! `hotl-sbx` — the launcher that applies the `win-writerestricted` floor.
//!
//! # Why a separate binary at all
//!
//! Linux rides Landlock into the child through a `pre_exec` closure — post-fork,
//! in the child. Windows has no fork, and `std::process::Command` has no stable
//! way to attach a primary token (`CommandExt` exposes `creation_flags`,
//! `startupinfo_force_feedback` and `async_pipes`, and an *unstable*
//! `raw_attribute` — nothing for a token). **There is no in-`Command` seam.**
//!
//! So `build_command` returns a `Command` for this program, which does the
//! token/job/desktop work in its own process and then `CreateProcessAsUserW`s
//! the real child. That is structurally identical to the macOS path, where
//! `build_command` returns a `Command` for `/usr/bin/sandbox-exec` and the
//! confinement happens in that helper. Only the arm inside the existing `match`
//! is new.
//!
//! # The non-widening property
//!
//! Per `CreateRestrictedToken`, if the existing token is **already** restricted,
//! the new restricting-SID list is the *intersection* of the two. A sandboxed
//! child that re-invokes this shim therefore cannot escape — it can only
//! narrow. Worth stating because it is the property that makes the shim safe to
//! leave on disk and on the child's PATH-adjacent directory.
//!
//! # Fail-closed shape
//!
//! Linux fails closed *post-fork*, by returning `ENOSYS` from `pre_exec` so the
//! child refuses to exec. This fails closed *pre-`CreateProcessAsUserW`*: every
//! error path exits `HOTL_SBX_REFUSED` **having spawned nothing**. The caller
//! maps that code to "the sandbox refused to start" rather than "your command
//! failed" — presenting a broken sandbox as a broken command is how an operator
//! stops trusting the label.
//!
//! # Status: UNVERIFIED
//!
//! This has never run. `hotl_tools::winfloor::spikes_retired()` gates the whole
//! mechanism closed, so nothing reaches this binary in a shipped build yet.

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    {
        windows_main()
    }
    #[cfg(not(windows))]
    {
        eprintln!(
            "hotl-sbx applies the Windows write floor and has no meaning on this platform; \
             the Unix floors are Seatbelt and Landlock, applied in-process."
        );
        std::process::ExitCode::from(hotl_tools::winfloor::HOTL_SBX_REFUSED as u8)
    }
}

#[cfg(windows)]
fn windows_main() -> std::process::ExitCode {
    let refused = || std::process::ExitCode::from(hotl_tools::winfloor::HOTL_SBX_REFUSED as u8);
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("hotl-sbx: nothing to run");
        return refused();
    }
    match imp::spawn_confined(&args) {
        Ok(code) => std::process::ExitCode::from((code & 0xff) as u8),
        Err(why) => {
            // The message goes to the child's stderr, which the caller captures
            // — so a refusal is legible in the tool output rather than only in
            // an exit code.
            eprintln!("hotl-sbx: refusing to start the child: {why}");
            refused()
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, SID_AND_ATTRIBUTES, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DUPLICATE, TOKEN_QUERY, WRITE_RESTRICTED,
    };
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_BASIC_UI_RESTRICTIONS,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
        JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES,
        JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
        JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
        InitializeProcThreadAttributeList, OpenProcessToken, UpdateProcThreadAttribute,
        WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS,
        EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_JOB_LIST, STARTUPINFOEXW,
    };

    /// Build the confined child and wait for it.
    ///
    /// Every step that can fail returns `Err` **before** anything is spawned,
    /// which is what makes the refusal total rather than partial.
    pub(super) fn spawn_confined(args: &[String]) -> Result<u32, String> {
        // 1. Our own token, with the three rights the restriction needs.
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: a pseudo-handle for our own process and a live out-param.
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
                &mut token,
            )
        } == 0
        {
            return Err(format!("OpenProcessToken: {}", last_error()));
        }

        // 2. The restricted token. `WRITE_RESTRICTED` narrows the second access
        //    check to writes; `DISABLE_MAX_PRIVILEGE` strips every privilege
        //    *except* `SeChangeNotifyPrivilege`, which is kept deliberately so
        //    bypass-traverse-checking survives and ancestor traverse rights
        //    never become a compatibility problem. (It is also why the
        //    NULL-DACL residual hole in `docs/SECURITY.md` stands — do not
        //    "fix" one without re-reading the other.)
        let mut sids = restricting_sids()?;
        let mut restricted: HANDLE = ptr::null_mut();
        // SAFETY: `sids` outlives the call; the counts match its length; every
        // list we are not supplying is zero-length and null.
        let ok = unsafe {
            CreateRestrictedToken(
                token,
                DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
                0,
                ptr::null(),
                0,
                ptr::null(),
                sids.len() as u32,
                sids.as_mut_ptr(),
                &mut restricted,
            )
        };
        // SAFETY: a handle we opened and no longer need.
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Err(format!("CreateRestrictedToken: {}", last_error()));
        }

        // 3. A job object that reaps the whole tree, including if *we* die.
        // SAFETY: an anonymous job with default security.
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Err(format!("CreateJobObjectW: {}", last_error()));
        }
        configure_job(job)?;

        // 4. `PROC_THREAD_ATTRIBUTE_JOB_LIST` — the whole reason this is a
        //    separate program. Assignment happens **before the initial thread
        //    runs**, which closes the spawn/adopt race that
        //    `TreeReaper::ADOPTION_IS_ATOMIC` reports as open everywhere else.
        let mut size = 0usize;
        // SAFETY: the deliberate probe call that reports the required size.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
        let mut attr_buf = vec![0u8; size];
        let attr_list = attr_buf.as_mut_ptr() as *mut _;
        // SAFETY: a buffer of exactly the size the OS asked for.
        if unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut size) } == 0 {
            return Err(format!(
                "InitializeProcThreadAttributeList: {}",
                last_error()
            ));
        }
        let job_ptr = &raw const job;
        // SAFETY: `job` outlives the `CreateProcessAsUserW` below, which is the
        // only consumer of this list.
        if unsafe {
            UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                job_ptr.cast(),
                size_of::<HANDLE>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(format!("UpdateProcThreadAttribute: {}", last_error()));
        }

        // 5. `DETACHED_PROCESS`: no console at all, so the child cannot call
        //    `WriteConsoleInput` on our `CONIN$` — the Windows shape of the
        //    TIOCSTI attack. stdio is inherited, so output still reaches the
        //    caller's pipe.
        // SAFETY: zeroed is a valid `STARTUPINFOEXW`; the fields set below are
        // the ones the flags declare.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attr_list;

        // SAFETY: zeroed is valid; `CreateProcessAsUserW` fills it in.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let mut cmdline = command_line(args);

        // SAFETY: `restricted` is a primary token derived from our own — which
        // is precisely why this needs no `SE_ASSIGNPRIMARYTOKEN_NAME` — and
        // `cmdline` is a live, NUL-terminated, mutable buffer as the API
        // requires.
        let spawned = unsafe {
            CreateProcessAsUserW(
                restricted,
                ptr::null(),
                cmdline.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT,
                ptr::null(),
                ptr::null(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        if spawned == 0 {
            return Err(format!("CreateProcessAsUserW: {}", last_error()));
        }

        // 6. Wait. If *we* die instead, the job handle closes and
        //    `KILL_ON_JOB_CLOSE` reaps the whole tree — stronger than
        //    `kill(-pgid)`, which does not survive its own caller.
        // SAFETY: a process handle the spawn just gave us.
        if unsafe { WaitForSingleObject(pi.hProcess, INFINITE) } != WAIT_OBJECT_0 {
            return Err(format!("WaitForSingleObject: {}", last_error()));
        }
        let mut code = 0u32;
        // SAFETY: same handle, and a live out-param.
        if unsafe { GetExitCodeProcess(pi.hProcess, &mut code) } == 0 {
            return Err(format!("GetExitCodeProcess: {}", last_error()));
        }
        Ok(code)
    }

    fn configure_job(job: HANDLE) -> Result<(), String> {
        // SAFETY: zeroed is a valid instance of this plain struct.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        // SAFETY: a job handle we own and a correctly-sized info struct.
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(format!("SetInformationJobObject(limits): {}", last_error()));
        }

        // Microsoft's own warning on `CreateRestrictedToken`: a restricted
        // application should not share a desktop with unrestricted ones, or it
        // can drive them with `SendMessage`/`PostMessage`. These UI
        // restrictions are the portable half of that defense; the alternate
        // desktop is the other half and is gated on spike R6.
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
        };
        // SAFETY: as above.
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectBasicUIRestrictions,
                (&raw const ui).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            )
        } == 0
        {
            return Err(format!("SetInformationJobObject(ui): {}", last_error()));
        }
        Ok(())
    }

    /// `{S_hotl, S_ws}` and nothing else — see the module doc on why `Everyone`
    /// is excluded.
    fn restricting_sids() -> Result<Vec<SID_AND_ATTRIBUTES>, String> {
        Err(
            "the restricting SIDs are derived by `DeriveAppContainerSidFromAppContainerName`, \
             which this build does not yet call: the derivation and its ACL pass are gated on \
             plan 0027's R1-R3 spikes. Refusing rather than spawning with an empty restricting \
             list, which would be an unconfined child wearing a confined label."
                .into(),
        )
    }

    /// argv → one quoted command line, per the `CommandLineToArgvW` rules
    /// `CreateProcess` parses with.
    fn command_line(args: &[String]) -> Vec<u16> {
        let mut out = String::new();
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push('"');
            let mut backslashes = 0usize;
            for c in arg.chars() {
                match c {
                    '\\' => {
                        backslashes += 1;
                        out.push('\\');
                    }
                    '"' => {
                        // Every backslash immediately before a quote must be
                        // doubled, and the quote escaped.
                        for _ in 0..=backslashes {
                            out.push('\\');
                        }
                        backslashes = 0;
                        out.push('"');
                    }
                    _ => {
                        backslashes = 0;
                        out.push(c);
                    }
                }
            }
            // Trailing backslashes would escape the closing quote.
            for _ in 0..backslashes {
                out.push('\\');
            }
            out.push('"');
        }
        OsStr::new(&out)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn last_error() -> std::io::Error {
        std::io::Error::last_os_error()
    }
}

#[cfg(test)]
mod tests {
    /// This binary has to reach the user's disk *next to* `hotl`, because
    /// `winfloor::shim_path()` resolves it from `current_exe().parent()` by
    /// absolute path and never through `PATH`. `dist` packages what cargo
    /// metadata reports, so the declaration is what makes that happen — and
    /// dropping it would not break any build, only the archive.
    #[test]
    fn the_package_declares_this_binary_so_dist_ships_it() {
        let manifest = include_str!("../../Cargo.toml");
        assert!(
            manifest.contains("name = \"hotl-sbx\""),
            "the [[bin]] declaration is gone; `dist` would ship `hotl` alone and the floor \
             would report Unavailable for a reason that reads like a bug"
        );
        assert!(
            manifest.contains("name = \"hotl\""),
            "the primary binary must stay declared alongside it"
        );
    }

    /// The Unix floors are in-process (Seatbelt, Landlock), so on those targets
    /// this binary is a stub that refuses — yet without the per-target override
    /// dist packaged it into every archive, and the shell installer put a
    /// ~330 KiB dead executable on Unix users' PATH. The override confines it
    /// to the Windows archive while the `[[bin]]` above keeps it building (and
    /// `cargo check`-covered) everywhere.
    #[test]
    fn dist_ships_this_binary_in_the_windows_archive_alone() {
        let manifest = include_str!("../../Cargo.toml");
        assert!(
            manifest.contains("[package.metadata.dist.binaries]"),
            "the per-target binaries override is gone; every Unix archive would carry the \
             hotl-sbx stub again, and the shell installer would install it on PATH"
        );
        assert!(
            manifest.contains("\"*\" = [\"hotl\"]"),
            "the wildcard arm must ship `hotl` alone everywhere the Windows arm does not name"
        );
        assert!(
            manifest.contains("x86_64-pc-windows-msvc = [\"hotl\", \"hotl-sbx\"]"),
            "the Windows arm must keep shipping the shim beside `hotl`, or \
             `winfloor::shim_path()` finds nothing and the floor reports Unavailable"
        );
    }
}
