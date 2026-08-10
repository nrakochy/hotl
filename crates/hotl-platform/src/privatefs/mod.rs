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

    /// Create and open a new private file. Fails if `path` exists.
    fn create_file_new(&self, path: &Path) -> io::Result<File>;

    /// Tighten an object that already exists. The one place a create-then-set
    /// window is unavoidable, so it is a named operation rather than the
    /// default path.
    fn harden_existing(&self, path: &Path) -> io::Result<()>;

    /// What the OS *actually* grants, read back from the object.
    fn effective_access(&self, path: &Path) -> io::Result<EffectiveAccess>;
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
    drop(fs.create_file_new(&file).unwrap());
    assert!(fs.effective_access(&file).unwrap().owner_only);

    // `O_EXCL`-shaped: an existing path is an error, never a truncation.
    std::fs::write(&file, b"payload").unwrap();
    assert_eq!(
        fs.create_file_new(&file).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"payload");

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
