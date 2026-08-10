//! [`DirHandle`] — the syscall layer under `fsguard`'s containment descent.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::UnixDirHandle;
#[cfg(unix)]
pub type ActiveDirHandle = UnixDirHandle;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::WindowsDirHandle;
#[cfg(windows)]
pub type ActiveDirHandle = WindowsDirHandle;

/// A directory the caller already proved is inside the guard root, and the
/// *only* way to go one step deeper.
///
/// INVARIANT, enforced by the type and not by review: **no method accepts an
/// absolute path, and no method accepts a multi-component path.** Every
/// operation names one component, relative to `self`. That is what makes
/// `fsguard`'s one-door property structural — code handed a `DirHandle` still
/// cannot reach outside it, which is strictly stronger than free functions,
/// where nothing but discipline stops a second `open()` on a full path.
///
/// The one exception proves the rule: [`resolve_beneath`](DirHandle::resolve_beneath)
/// takes a multi-component *relative* path, and the whole point of it is that
/// the **kernel** enforces beneath-ness in a single syscall. It is not a
/// weakening of the invariant, it is the invariant delegated to the one place
/// that can honor it atomically.
///
/// Sealed, because the guarantees above are statements about *all* implementors.
pub trait DirHandle: Sized + Send + Sync + crate::sealed::Sealed {
    /// Attributes worth carrying across an atomic replace. Unix: the mode, so
    /// editing a script does not silently drop its executable bit. Windows: the
    /// `FILE_ATTRIBUTE_*` bits, where read-only and hidden play the same role.
    /// Not a shared shape, because there is no shared shape to have.
    type Attrs: Copy;

    /// Open the guard root itself, by name. The root is ours, not the model's —
    /// this is the only place a full path enters, and it is the anchor every
    /// other method is relative to.
    fn open_root(path: &Path) -> Result<Self, GuardIo>;

    /// Open a child directory without following any link at the final
    /// component. Unix `O_NOFOLLOW|O_DIRECTORY`; Windows
    /// `FILE_OPEN_REPARSE_POINT|FILE_DIRECTORY_FILE` relative to the handle.
    fn open_child_dir(&self, name: &OsStr) -> Result<Self, GuardIo>;

    /// `mkdirat`. Succeeding when it already exists is the caller's business,
    /// not this method's.
    fn make_child_dir(&self, name: &OsStr) -> Result<(), GuardIo>;

    fn open_child_file(&self, name: &OsStr, mode: OpenMode) -> Result<File, GuardIo>;
    fn create_child_file(&self, name: &OsStr, excl: Excl) -> Result<File, GuardIo>;
    fn rename_child(&self, from: &OsStr, to: &OsStr) -> Result<(), GuardIo>;
    /// Best-effort removal of our own temp file. A failure here has nothing to
    /// report, which is why it returns nothing.
    fn unlink_child(&self, name: &OsStr);

    /// Classify a child **without following** it, or `None` if it does not
    /// exist. See [`NodeKind`] — the fail-closed default is the whole point.
    fn child_kind(&self, name: &OsStr) -> Option<NodeKind>;

    fn child_attrs(&self, name: &OsStr) -> Option<Self::Attrs>;
    fn apply_attrs(&self, file: &File, attrs: Self::Attrs) -> io::Result<()>;

    /// Stable identity, for the belt-and-braces check that the handle we
    /// validated and the path we return are the same object.
    fn identity(&self) -> Result<NodeId, GuardIo>;

    /// Single-syscall beneath-resolution where the OS has one: Linux `openat2`
    /// with `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS`,
    /// Windows `NtCreateFile` with `OBJ_DONT_REPARSE`.
    ///
    /// `Ok(None)` means **this kernel does not offer it** (pre-5.6 Linux, a
    /// seccomp filter returning ENOSYS/EPERM, pre-1607 Windows) and the caller
    /// must fall through to the component-wise descent. It does **not** mean
    /// "allowed" — an actual refusal is `Err`.
    fn resolve_beneath(&self, rel: &Path, mode: OpenMode) -> Result<Option<File>, GuardIo>;

    /// Hand back the handle for this directory itself, for the one case where
    /// the caller's target *is* the directory it already descended to.
    /// Widens nothing: it returns the object the caller already holds.
    fn into_file(self) -> File;

    /// After this returns, the most recent rename in this directory survives a
    /// crash.
    ///
    /// CONTRACT is the *guarantee*, not the mechanism, and the two platforms
    /// reach it differently: Unix must `fsync` the directory fd, because
    /// without it the rename can be lost even though the data was synced. NTFS
    /// journals the metadata operation itself, so on Windows the guarantee
    /// already holds when the rename returns. Windows returning `Ok` is
    /// therefore a claim that the property is satisfied — not a no-op standing
    /// in for a capability the platform lacks.
    fn sync_name_durability(&self) -> Result<(), GuardIo>;
}

/// A failed syscall, plus the one classification only the adapter can make.
#[derive(Debug)]
pub struct GuardIo {
    pub error: io::Error,
    /// The OS refused because a component was a link or other reparse point,
    /// as distinct from any other failure.
    ///
    /// Each adapter decides this from its own quirks — POSIX says `ELOOP` but
    /// macOS says `ENOTDIR` whenever `O_DIRECTORY` is also set, and Windows
    /// says `STATUS_REPARSE_POINT_ENCOUNTERED` or reports the attribute on a
    /// handle it opened without following. That is translation, not policy, so
    /// it belongs in the adapter; what the caller *does* about it is policy,
    /// and stays in `fsguard`.
    pub refused_a_link: bool,
}

impl GuardIo {
    pub fn io(error: io::Error) -> Self {
        Self {
            error,
            refused_a_link: false,
        }
    }

    pub fn link(error: io::Error) -> Self {
        Self {
            error,
            refused_a_link: true,
        }
    }

    pub fn last_os_error() -> Self {
        Self::io(io::Error::last_os_error())
    }
}

impl From<io::Error> for GuardIo {
    fn from(error: io::Error) -> Self {
        Self::io(error)
    }
}

/// What a child is, decided without following it.
///
/// `#[non_exhaustive]`, and every reader must match with a catch-all arm that
/// **refuses**. A variant added later for diagnostics has to fail closed on
/// every existing reader rather than fall into an `is_dir()`-shaped assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeKind {
    Dir,
    RegularFile,
    /// Symlink, NTFS junction, app-exec-link, WCI layer, OneDrive placeholder,
    /// or any other reparse tag.
    ///
    /// **One variant on purpose.** The Windows trap is that a junction reports
    /// `is_symlink() == false` and `is_dir() == true`, so any type that lets a
    /// caller tell "symlink" from "some other reparse point" invites the wrong
    /// branch. There is no branch here to get wrong.
    NotFollowable {
        tag: Option<u32>,
    },
    /// FIFO, socket, device, `NUL`, a named pipe — anything a read would block
    /// on, or that is not a file at all.
    NotAFile,
}

/// Device plus file identity.
///
/// 128 bits for the file half because ReFS needs it; Unix fills the low half
/// with the inode. Compared, never interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeId {
    pub volume: u64,
    pub file: u128,
}

/// How a leaf is opened for reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// A regular file. Unix adds `O_NONBLOCK` so a FIFO cannot block the open
    /// *before* the caller gets a chance to refuse it — see [`unblock`].
    File,
    /// A directory, for a walk root.
    Dir,
}

/// Whether a create may land on an existing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Excl {
    /// `O_TRUNC` — replace the contents of whatever is there.
    Truncate,
    /// `O_EXCL` — fail if anything is there at all.
    MustNotExist,
}

/// Stable identity of an open handle, for the belt-and-braces check that the
/// handle the guard validated and the path it returns name the same object.
pub fn identity_of(file: &File) -> io::Result<NodeId> {
    #[cfg(unix)]
    {
        unix::identity_of(file)
    }
    #[cfg(windows)]
    {
        windows::identity_of(file)
    }
}

/// The same identity, taken by name **without following a final link**. The
/// other half of the comparison above.
pub fn identity_at(path: &Path) -> io::Result<NodeId> {
    #[cfg(unix)]
    {
        unix::identity_at(path)
    }
    #[cfg(windows)]
    {
        windows::identity_at(path)
    }
}

/// The OS's own normalized name for an open handle.
///
/// `Ok(None)` where the platform has no such call — Unix, where `/proc/self/fd`
/// resolves through the very magic links the guard refuses and so is not the
/// same answer. Windows uses
/// `GetFinalPathNameByHandleW(FILE_NAME_NORMALIZED|VOLUME_NAME_DOS)`, which
/// catches 8.3 short names, case, and trailing dot/space forms in one call.
pub fn normalized_name(file: &File) -> io::Result<Option<PathBuf>> {
    #[cfg(unix)]
    {
        let _ = file;
        Ok(None)
    }
    #[cfg(windows)]
    {
        windows::normalized_name(file)
    }
}

/// Undo [`OpenMode::File`]'s non-blocking open once the handle is known to be a
/// regular file.
///
/// Windows is a genuine no-op here, and that is a statement about the platform
/// rather than a gap: `CreateFile` on a named pipe does not block waiting for a
/// peer the way `open(2)` on a FIFO blocks waiting for a writer, so there was
/// never a flag to set and there is nothing to clear.
pub fn unblock(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        unix::clear_nonblock(file)
    }
    #[cfg(windows)]
    {
        let _ = file;
        Ok(())
    }
}

/// The lexical refusals, applied to one path component before it is ever
/// handed to the OS.
///
/// These are Windows filename semantics, and they are checked on **every**
/// platform on purpose: a deny rule, a glob and an `execute_later_reason`
/// match all compare the name the model wrote, while Win32 opens something
/// else. Refusing the divergent forms outright is cheaper than teaching four
/// matchers about them, and a name hotl refuses is a name no matcher can be
/// wrong about.
///
/// Returns the reason, or `None` when the component is fine.
pub fn refuse_component(name: &OsStr) -> Option<&'static str> {
    let Some(text) = name.to_str() else {
        // Not UTF-8: nothing below can be reasoned about, and every caller
        // treats an unnameable component as an escape.
        return Some("is not valid UTF-8");
    };
    if text.is_empty() {
        return Some("is empty");
    }
    // An NTFS alternate data stream. `Path::components()` does not split on
    // `:`, so a matcher sees `agents.md:evil` as one name while Win32 opens the
    // `evil` stream of `agents.md` — the deny rule and the write disagree about
    // which object is involved.
    if text.contains(':') {
        return Some("names an alternate data stream (`:`)");
    }
    // Win32 strips these and opens the stripped name, so `AGENTS.md.` writes
    // `AGENTS.md` while every matcher sees a different file.
    if text.ends_with('.') || text.ends_with(' ') {
        return Some("ends with a dot or space, which Windows silently strips");
    }
    if is_reserved_device_name(text) {
        return Some("is a reserved device name");
    }
    None
}

/// `CON`, `NUL`, `AUX`, `PRN`, `COM1-9`, `LPT1-9` — including with an
/// extension (`CON.txt`) and in any directory, because Win32 redirects the name
/// to a device wherever it appears.
fn is_reserved_device_name(text: &str) -> bool {
    let stem = text
        .split('.')
        .next()
        .unwrap_or(text)
        .trim_end_matches([' ', '.']);
    if matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let upper = stem.to_ascii_uppercase();
    for prefix in ["COM", "LPT"] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            if rest.len() == 1 && matches!(rest.as_bytes()[0], b'1'..=b'9') {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked on every platform, so the assertions are too. A name hotl
    /// refuses is a name no path matcher can disagree with Win32 about.
    #[test]
    fn the_lexical_refusals_hold_on_every_platform() {
        for bad in [
            "AGENTS.md:evil",
            "AGENTS.md.",
            "AGENTS.md ",
            "CON",
            "con.txt",
            "NUL",
            "COM1",
            "lpt9.log",
            "",
        ] {
            assert!(
                refuse_component(OsStr::new(bad)).is_some(),
                "`{bad}` must be refused"
            );
        }
        // And the near-misses are not over-refused: these are ordinary names.
        for ok in [
            "AGENTS.md",
            "console.log",
            "COM0",
            "COM10",
            "nulls.rs",
            "a.b.c",
            "LPTX",
        ] {
            assert_eq!(
                refuse_component(OsStr::new(ok)),
                None,
                "`{ok}` must be allowed"
            );
        }
    }

    /// One body, both syscall backends: the root opens, a child directory and
    /// a child file round-trip, and identity distinguishes two objects.
    #[test]
    fn active_adapter_upholds_the_contract() {
        use std::io::{Read, Write};
        let scratch = std::env::temp_dir().join(format!("hotl-dirhandle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(scratch.join("sub")).unwrap();
        std::fs::write(scratch.join("sub").join("f.txt"), b"hello").unwrap();

        let root = ActiveDirHandle::open_root(&scratch).unwrap();
        assert_eq!(root.child_kind(OsStr::new("sub")), Some(NodeKind::Dir));
        assert_eq!(root.child_kind(OsStr::new("nope")), None);

        let sub = root.open_child_dir(OsStr::new("sub")).unwrap();
        assert_eq!(
            sub.child_kind(OsStr::new("f.txt")),
            Some(NodeKind::RegularFile)
        );

        let mut f = sub
            .open_child_file(OsStr::new("f.txt"), OpenMode::File)
            .unwrap();
        unblock(&f).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");

        // Two different directories are two different objects.
        assert_ne!(root.identity().unwrap(), sub.identity().unwrap());

        // `MustNotExist` really refuses an existing name.
        assert!(sub
            .create_child_file(OsStr::new("f.txt"), Excl::MustNotExist)
            .is_err());
        let mut new = sub
            .create_child_file(OsStr::new("g.txt"), Excl::MustNotExist)
            .unwrap();
        new.write_all(b"g").unwrap();
        drop(new);

        // Rename and unlink are relative to the handle, both ways.
        sub.rename_child(OsStr::new("g.txt"), OsStr::new("h.txt"))
            .unwrap();
        assert_eq!(sub.child_kind(OsStr::new("g.txt")), None);
        sub.sync_name_durability().unwrap();
        sub.unlink_child(OsStr::new("h.txt"));
        assert_eq!(sub.child_kind(OsStr::new("h.txt")), None);

        let _ = std::fs::remove_dir_all(&scratch);
    }
}
