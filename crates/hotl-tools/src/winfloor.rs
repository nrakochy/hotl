//! `win-writerestricted` — the write floor for native Windows.
//!
//! # The primitive
//!
//! `CreateRestrictedToken` performs *two* access checks: one against the
//! token's enabled SIDs, one against its restricting-SID list. The
//! `WRITE_RESTRICTED` flag narrows the **second** check to write access only;
//! reads are unaffected. That is exactly hotl's shape:
//!
//! | invariant | behavior under a write-restricted token |
//! |---|---|
//! | writes only under cwd/temp/extras | second check fails everywhere except objects whose DACL names our SID |
//! | reads open except a carve | second check never runs for reads → whole FS readable, **and the carve is lost** |
//! | never widenable from inside | first check still applies |
//!
//! The lost carve is not a degradation to paper over: `reads:open` is
//! permanent on native Windows, and `docs/SECURITY.md` says so in the loudest
//! place it has. WSL2 remains the only Windows path with secret-read
//! confinement.
//!
//! # Why writing ACEs into a user's tree is safe
//!
//! Microsoft: restricting SIDs *"can only be used to restrain access that a
//! user account would have to start with but not broaden it in any event."* An
//! ACE naming one of our synthetic SIDs therefore **grants nothing to
//! anybody** — any principal that could use it must already pass the normal
//! user check. The ACEs are inert for every process on the machine except a
//! hotl-sandboxed child. This is the load-bearing safety property of the whole
//! design.
//!
//! # The two SIDs
//!
//! Both from `DeriveAppContainerSidFromAppContainerName`, a pure SHA-256
//! derivation that creates **no profile and no persistent state**. Do *not*
//! call `CreateAppContainerProfile`.
//!
//! - `S_hotl` = derive("hotl.sandbox.v1") — machine-stable, the **deny** side.
//! - `S_ws` = derive("hotl.ws." + hex(sha256(canonical cwd))) — the **allow**
//!   side, so a hotl session in workspace A cannot write workspace B even
//!   though both children hold a hotl token.
//!
//! `SidsToRestrict` is `{S_hotl, S_ws}` and **nothing else** — in particular
//! not `Everyone` (S-1-1-0). Microsoft's own `MpsSvc` includes it, which is
//! why that floor leaks to `C:\Users\Public`, `C:\ProgramData` and
//! `C:\Windows\Temp`. Excluding it is what makes this one tight.
//!
//! # Status: UNVERIFIED
//!
//! None of this has run. [`spikes_retired`] gates it closed, and
//! [`mechanism_available`] refuses until that gate opens — see its doc for the
//! experiments that have to come back first.

#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;

/// Have the plan's floor spikes been run on real hardware and their results
/// recorded?
///
/// **`false`, deliberately, and it must stay false until someone runs them.**
/// The code below is written but has never executed; certifying a mechanism
/// nobody observed working is the exact failure the sandbox probe's whole
/// design exists to prevent, and it would be worse here than anywhere else
/// because the label would read `sandboxed:win-writerestricted` while
/// confining nothing.
///
/// What has to come back first:
///
/// - **R1** — *which* access masks trip `WRITE_RESTRICTED`'s second check.
///   Microsoft documents *that* it narrows to "write access" but not which
///   mask bits qualify. This is the Windows twin of the Landlock `TRUNCATE`
///   gap that cost hotl a real hole. Probe create/write/append/`SetFileTime`/
///   `DeleteFile`/`MoveFileEx`/`SetFileSecurity`/`WRITE_DAC`/`WRITE_OWNER`/
///   `FILE_WRITE_ATTRIBUTES`/`FILE_WRITE_EA` on an un-ACEd file and record
///   which fail. Any that *succeed* are silent holes.
/// - **R2** — does excluding `Everyone` from `SidsToRestrict` break process
///   startup? Run `cmd`, `git`, `cargo build`, `node`, `python`, `msbuild`
///   under `{S_hotl, S_ws}` and watch for failures creating named objects in
///   `\Sessions\N\BaseNamedObjects` or connecting to CSRSS.
/// - **R3** — does the child reach WMI, DCOM or the SCM? Microsoft documents
///   that `Win32_Process.Create` children **escape job objects**. Any success
///   is a full escape.
///
/// R2 or R3 coming back badly is the signal to ship `Unavailable` permanently
/// rather than a floor that reads stronger than it is. That outcome is
/// acceptable: the rest of the port still delivers a compiling, tested,
/// usable native Windows build with an honest fail-closed posture.
pub const fn spikes_retired() -> bool {
    false
}

/// The mechanism name. Registered here so `net::kernel_backing`'s catch-all
/// produces `NET:UNENFORCED(\`win-writerestricted\` cannot confine the
/// network)` without any change to `net.rs` — which is the correct answer,
/// permanently: a restricted token cannot filter packets.
pub const MECHANISM: &str = "win-writerestricted";

/// `%LOCALAPPDATA%\hotl\windows-acl.json` — the journal of which roots have
/// been ACEd, so a partially-applied pass is visible rather than assumed
/// complete.
pub fn journal_path() -> Option<PathBuf> {
    use hotl_platform::KnownPaths as _;
    hotl_platform::KNOWN_PATHS
        .data()
        .map(|d| d.join("windows-acl.json"))
}

/// Is the floor available *right now*?
///
/// Each failure gets its own actionable reason, because "unavailable" with no
/// cause is a dead end for whoever hits it. In order:
///
/// 1. The spikes are unretired — see [`spikes_retired`].
/// 2. The workspace volume must support ACLs (`FILE_PERSISTENT_ACLS`). An
///    exFAT USB drive or an ACL-less SMB share must yield `Unavailable`, not a
///    silently absent floor.
/// 3. The consent marker must be present: hotl does not rewrite security
///    descriptors across a user's repo without being asked.
/// 4. `hotl-sbx.exe` must exist next to `current_exe()`. Missing means
///    `Unavailable` — never a fallback to an unconfined spawn.
pub fn mechanism_available() -> Result<&'static str, String> {
    if !spikes_retired() {
        return Err(
            "the Windows write floor is implemented but has never been run on real hardware, so \
             hotl will not certify it. Until its spikes are recorded, every exec is individually \
             gated and allow-rules do not persist. Use WSL2 for a confined session."
                .into(),
        );
    }
    #[cfg(windows)]
    {
        let cwd = std::env::current_dir().map_err(|e| format!("cannot read the cwd: {e}"))?;
        if !volume_supports_acls(&cwd)? {
            return Err(format!(
                "the volume holding {} does not support persistent ACLs (exFAT, or an ACL-less \
                 SMB share), so no write floor can be expressed there",
                cwd.display()
            ));
        }
        if !consent_given() {
            return Err(
                "the Windows floor needs to add an inert ACE under the workspace; run \
                 `hotl sandbox prepare`, or set [sandbox].windows_acl = \"auto\""
                    .into(),
            );
        }
        if shim_path().is_none() {
            return Err(
                "hotl-sbx.exe is missing next to the hotl binary; reinstall rather than run \
                 unconfined"
                    .into(),
            );
        }
        Ok(MECHANISM)
    }
    #[cfg(not(windows))]
    {
        Err("win-writerestricted is a Windows mechanism".into())
    }
}

/// The launcher, by **absolute path** from `current_exe().parent()` — never via
/// `PATH`.
///
/// `PATH` lookup would let anything earlier on it stand in for the component
/// that applies the floor, which is the one program that must not be
/// substitutable.
pub fn shim_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join(if cfg!(windows) {
        "hotl-sbx.exe"
    } else {
        "hotl-sbx"
    });
    candidate.is_file().then_some(candidate)
}

/// Whether the owner has consented to the ACL pass for this workspace.
pub fn consent_given() -> bool {
    match journal_path() {
        Some(p) => p.is_file(),
        None => false,
    }
}

/// The exit code the shim uses when it refuses to start a child.
///
/// Outside `PROBE_EXIT_BASE`'s range on purpose: `run_bounded` already treats
/// an out-of-range code as *unverifiable* rather than as an escape, and
/// `bash_impl`/`grep_search` map this one specifically to "the sandbox refused
/// to start" rather than "your command failed". Presenting a broken sandbox as
/// a broken command is how an operator stops trusting the label.
pub const HOTL_SBX_REFUSED: i32 = 199;

/// The write roots on Windows.
///
/// `/private/tmp` and `/dev` drop — they do not exist — and the real `%TEMP%`
/// is **narrowed rather than adopted**: it is huge, shared, and ACEing it would
/// be a genuinely surprising side effect. A per-session subdirectory is created
/// and ACEd instead (instant, because it is empty), and the child's
/// `TEMP`/`TMP`/`TMPDIR` are pointed at it.
///
/// Behavior difference worth stating out loud: a child that hardcodes
/// `C:\Users\x\AppData\Local\Temp\foo` is denied.
pub fn write_roots(cwd: PathBuf, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = vec![cwd];
    if let Some(t) = session_temp() {
        roots.push(t);
    }
    roots.extend(extra.iter().cloned());
    roots
}

/// `%TEMP%\hotl-<session>`, the narrow temp the child gets instead of the
/// shared one.
pub fn session_temp() -> Option<PathBuf> {
    let base = std::env::temp_dir();
    Some(base.join(format!("hotl-{}", std::process::id())))
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    /// `GetVolumeInformationW` → `FILE_PERSISTENT_ACLS`.
    pub(super) fn volume_supports_acls(path: &Path) -> Result<bool, String> {
        use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
        /// `FILE_PERSISTENT_ACLS`. Not exported by `windows-sys`, so declared
        /// here with its documented value: the volume preserves and enforces
        /// ACLs, which every other claim in this module depends on.
        const FILE_PERSISTENT_ACLS: u32 = 0x0000_0008;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut root = [0u16; 260];
        // SAFETY: a NUL-terminated path and a live out-buffer of the stated
        // length.
        if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return Err(format!(
                "cannot find the volume for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        let mut flags = 0u32;
        // SAFETY: the volume root we just resolved; every buffer we are not
        // asking for is null with a zero length.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return Err(format!(
                "cannot read volume information: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(flags & FILE_PERSISTENT_ACLS != 0)
    }
}

#[cfg(windows)]
use imp::volume_supports_acls;

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate is closed, and a test says so out loud.
    ///
    /// This is not busywork: the whole floor is written but unrun, and the one
    /// way it ships as a lie is somebody flipping `spikes_retired` without the
    /// spike results. A failing test at that moment is the cheapest possible
    /// reminder to paste the results into the plan's decision log first.
    #[test]
    fn the_floor_refuses_to_certify_itself_until_its_spikes_are_run() {
        assert!(
            !spikes_retired(),
            "R1-R3 have not been recorded in plan 0027's decision log. If you have run them, \
             record the results there in the same change that flips this."
        );
        let err = mechanism_available().unwrap_err();
        assert!(
            err.contains("never been run on real hardware"),
            "the refusal must say why, not just refuse: {err}"
        );
    }

    /// The mechanism name is what `net.rs` turns into `NET:UNENFORCED` without
    /// any change of its own — a restricted token cannot filter packets, and
    /// `kernel_backing`'s catch-all already produces exactly that answer for
    /// any mechanism it does not know how to back.
    #[test]
    fn the_mechanism_name_reads_as_a_write_only_floor() {
        assert_eq!(MECHANISM, "win-writerestricted");
        // `net.rs` needed no change to reach the right verdict here. If a
        // future edit adds `win-writerestricted` to its backed set, this fails
        // — which is the point, because the floor confines writes and nothing
        // else.
        let src = include_str!("net.rs");
        assert!(
            !src.contains("win-writerestricted"),
            "the egress layer must not learn about the write floor: its catch-all already \
             answers correctly, and a special case could only make it wrong"
        );
    }

    #[test]
    fn the_shim_is_never_resolved_through_path() {
        // The point is the absence of a `PATH` search: `shim_path` either finds
        // the launcher beside the binary or reports nothing, so no earlier
        // entry on `PATH` can stand in for the component that applies the
        // floor.
        let src = include_str!("winfloor.rs");
        let body = &src[src.find("pub fn shim_path").unwrap()..];
        let body = &body[..body.find("\n}").unwrap()];
        assert!(
            body.contains("current_exe"),
            "the shim must be resolved from current_exe()"
        );
        assert!(
            !body.contains("var(\"PATH\")") && !body.contains("var_os(\"PATH\")"),
            "the shim must never be resolved through PATH"
        );
    }
}
