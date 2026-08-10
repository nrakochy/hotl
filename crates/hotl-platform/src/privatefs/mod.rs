//! [`PrivateFs`] — filesystem objects only the current user can read.

use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::UnixPrivateFs;
#[cfg(unix)]
pub type ActivePrivateFs = UnixPrivateFs;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::owner_only_attributes;
#[cfg(windows)]
pub use windows::WindowsPrivateFs;
#[cfg(windows)]
pub type ActivePrivateFs = WindowsPrivateFs;

/// Create filesystem objects only the current user can read.
///
/// CONTRACT (all implementors): the restriction is applied **at create**, never
/// create-then-tighten. The session log is the most sensitive artifact hotl
/// writes, and a create-then-chmod window is a real read window.
/// [`create_file_new`](PrivateFs::create_file_new) is `O_EXCL`-shaped: it fails
/// if the path exists, and it never truncates.
///
/// NOT EQUAL ACROSS PLATFORMS, by construction. `0600` excludes root only until
/// root chooses otherwise; a Windows DACL excludes local Administrators only
/// until they use `SeTakeOwnershipPrivilege`/`SeBackupPrivilege`. Comparable,
/// not identical. Two Windows-only caveats have no Unix analogue: a roaming or
/// redirected `%APPDATA%` on an SMB share defeats the DACL entirely, and there
/// is no umask, so a *pre-existing* directory keeps whatever it had.
/// [`effective_access`](PrivateFs::effective_access) exists so callers can check
/// rather than assume — never certify a mechanism you did not observe working.
pub trait PrivateFs: crate::sealed::Sealed {
    /// Create a directory readable only by the current user. Succeeds quietly
    /// if it already exists **and already excludes everyone else**; otherwise
    /// it tightens, because an inherited-permissions directory is the exact
    /// case `harden_existing` exists for.
    fn create_dir(&self, path: &Path) -> io::Result<()>;

    /// Create `path` and any missing ancestors.
    ///
    /// CONTRACT: **only the components this call creates are made private.** A
    /// component that already exists is left exactly as it is, and that is not
    /// laziness — the ancestors of a data dir are `$HOME` and `~/.local`, and
    /// silently tightening those would be a side effect far outside what a
    /// caller asking for one private directory consented to. A caller that
    /// wants an existing object narrowed asks
    /// [`harden_existing`](PrivateFs::harden_existing) by name.
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        if path.as_os_str().is_empty() {
            return Ok(());
        }
        if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
            return Ok(());
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            self.create_dir_all(parent)?;
        }
        match self.create_dir(path) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }

    /// Create and open a new private file. Fails if `path` exists.
    fn create_file_new(&self, path: &Path, writes: Writes) -> io::Result<File>;

    /// Create a private file, truncating one that already exists.
    ///
    /// CONTRACT, and it is weaker than [`create_file_new`](PrivateFs::create_file_new)
    /// on **both** platforms: an at-create restriction only applies to a file
    /// the call actually creates. A Unix `mode` is ignored for an existing
    /// path, and a Windows `SECURITY_ATTRIBUTES` is ignored unless the
    /// disposition creates. So a pre-existing loose file is narrowed *after*
    /// the open, and that window is real. Use `create_file_new` wherever the
    /// path is known to be fresh; this exists for content-addressed blobs,
    /// where a rewrite rewrites identical bytes.
    fn create_file_truncate(&self, path: &Path) -> io::Result<File>;

    /// Tighten an object that already exists. The one place a create-then-set
    /// window is unavoidable, so it is a named operation rather than the
    /// default path.
    fn harden_existing(&self, path: &Path) -> io::Result<()>;

    /// What the OS *actually* grants, read back from the object.
    fn effective_access(&self, path: &Path) -> io::Result<EffectiveAccess>;
}

/// How the returned handle writes.
///
/// `Append` is `O_APPEND` / `FILE_APPEND_DATA`: every write lands at the
/// current end of file. That is not a convenience — it is what keeps an
/// append-only log append-only when a writer thread and a reader disagree
/// about the offset, and it is why this is a parameter rather than something
/// the caller arranges with a seek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    FromStart,
    Append,
}

/// Read back from the object, not inferred from what we asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAccess {
    pub owner_only: bool,
    /// Principals other than the owner that can read it, named for a human —
    /// a mode string on Unix, a resolved account name on Windows.
    pub other_readers: Vec<String>,
}

/// One test body per contract clause, run against whichever adapter this build
/// selected (rule 8). `windows.rs` adds the one assertion that has no Unix
/// counterpart.
#[cfg(test)]
pub(crate) fn assert_private_fs_contract<P: PrivateFs>(fs: &P, scratch: &Path) {
    let dir = scratch.join("private-dir");
    fs.create_dir(&dir).unwrap();
    let access = fs.effective_access(&dir).unwrap();
    assert!(
        access.owner_only,
        "a freshly created private dir must exclude everyone else, got {:?}",
        access.other_readers
    );

    let file = dir.join("secret");
    drop(fs.create_file_new(&file, Writes::FromStart).unwrap());
    assert!(fs.effective_access(&file).unwrap().owner_only);

    // `O_EXCL`-shaped: an existing path is an error, never a truncation.
    std::fs::write(&file, b"payload").unwrap();
    assert_eq!(
        fs.create_file_new(&file, Writes::FromStart)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"payload");

    // `create_file_truncate` narrows a pre-existing loose file rather than
    // inheriting its permissions.
    let blob = dir.join("blob");
    std::fs::write(&blob, b"old").unwrap();
    loosen(&blob);
    {
        use std::io::Write as _;
        let mut f = fs.create_file_truncate(&blob).unwrap();
        f.write_all(b"new").unwrap();
    }
    assert_eq!(std::fs::read(&blob).unwrap(), b"new");
    assert!(fs.effective_access(&blob).unwrap().owner_only);

    // `Append` really appends rather than overwriting from offset zero.
    let logfile = dir.join("log");
    {
        use std::io::Write as _;
        let mut a = fs.create_file_new(&logfile, Writes::Append).unwrap();
        a.write_all(b"one").unwrap();
        a.write_all(b"two").unwrap();
    }
    assert_eq!(std::fs::read(&logfile).unwrap(), b"onetwo");

    // Loosening and re-hardening is the `harden_existing` path.
    loosen(&file);
    fs.harden_existing(&file).unwrap();
    assert!(fs.effective_access(&file).unwrap().owner_only);
}

#[cfg(all(test, unix))]
fn loosen(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[cfg(all(test, windows))]
fn loosen(path: &Path) {
    // Re-enable inheritance, which is how a Windows object picks up readers it
    // was not created with. `harden_existing` must put `SE_DACL_PROTECTED`
    // back.
    windows::allow_inheritance(path).unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn active_adapter_upholds_the_contract() {
        let scratch =
            std::env::temp_dir().join(format!("hotl-privatefs-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&scratch).unwrap();
        super::assert_private_fs_contract(&crate::PRIVATE_FS, &scratch);
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
