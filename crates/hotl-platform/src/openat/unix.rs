//! `openat`/`mkdirat`/`fstatat`/`unlinkat`/`renameat` from a directory fd,
//! plus Linux's `openat2` as the single-syscall path.
//!
//! This is the code `fsguard` carried inline before there was a second
//! platform; the behavior is unchanged.

use super::{DirHandle, Excl, GuardIo, NodeId, NodeKind, OpenMode};
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::Path;

pub struct UnixDirHandle(File);

impl crate::sealed::Sealed for UnixDirHandle {}

const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY;

/// `O_NONBLOCK` is not decoration: `open(2)` on a FIFO blocks until a writer
/// appears, so without it the guard would hang *before* it ever got the chance
/// to refuse the FIFO. [`super::unblock`] clears it once the handle is known to
/// be a regular file.
const FILE_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;

fn cstr(name: &OsStr) -> Result<CString, GuardIo> {
    CString::new(name.as_bytes())
        .map_err(|_| GuardIo::io(io::Error::from(io::ErrorKind::InvalidInput)))
}

/// POSIX says `O_NOFOLLOW` on a symlink is `ELOOP`, but macOS returns `ENOTDIR`
/// whenever `O_DIRECTORY` is also set, and an intermediate link can surface
/// either. So classify by asking the *same dirfd* whether the component is a
/// link. The open has already failed — this only picks the classification, and
/// nothing is ever opened on its strength, so it is not a check-then-open.
fn classify_failure(dir: &File, name: &OsStr, err: io::Error) -> GuardIo {
    let looks_like_a_link = matches!(err.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
        && matches!(
            kind_at(dir, name),
            Some(NodeKind::NotFollowable { .. }) | Some(NodeKind::NotAFile)
        );
    if looks_like_a_link {
        GuardIo::link(err)
    } else {
        GuardIo::io(err)
    }
}

fn stat_at(dir: &File, name: &OsStr) -> Option<libc::stat> {
    let c = cstr(name).ok()?;
    // SAFETY: `stat` is a plain repr(C) struct; all-zero is a valid instance.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is an open directory fd we own; `c` is NUL-terminated.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    (rc == 0).then_some(st)
}

fn kind_at(dir: &File, name: &OsStr) -> Option<NodeKind> {
    let st = stat_at(dir, name)?;
    Some(match st.st_mode & libc::S_IFMT {
        libc::S_IFDIR => NodeKind::Dir,
        libc::S_IFREG => NodeKind::RegularFile,
        libc::S_IFLNK => NodeKind::NotFollowable { tag: None },
        _ => NodeKind::NotAFile,
    })
}

pub(super) fn identity_of(file: &File) -> io::Result<NodeId> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(NodeId {
        volume: m.dev(),
        file: m.ino() as u128,
    })
}

pub(super) fn identity_at(path: &Path) -> io::Result<NodeId> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    Ok(NodeId {
        volume: m.dev(),
        file: m.ino() as u128,
    })
}

pub(super) fn clear_nonblock(file: &File) -> io::Result<()> {
    // SAFETY: plain fcntl(2) on a fd we own.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same fd, clearing one flag.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl DirHandle for UnixDirHandle {
    /// The full `st_mode`, so an atomic replace does not silently drop the
    /// executable bit off every script it edits.
    type Attrs = libc::mode_t;

    fn open_root(path: &Path) -> Result<Self, GuardIo> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| GuardIo::io(io::Error::from(io::ErrorKind::InvalidInput)))?;
        // The root is opened by name — it is ours, not the model's — but with
        // `O_DIRECTORY` so a swapped-out root fails loudly rather than silently
        // rebasing the whole guard.
        // SAFETY: plain open(2) on a NUL-terminated path we own.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
            )
        };
        if fd < 0 {
            return Err(GuardIo::last_os_error());
        }
        // SAFETY: `fd` was just opened and is owned by nothing else.
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }

    fn open_child_dir(&self, name: &OsStr) -> Result<Self, GuardIo> {
        let c = cstr(name)?;
        // SAFETY: `self.0` is an open directory fd we own; `c` is NUL-terminated.
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), DIR_FLAGS) };
        if fd < 0 {
            return Err(classify_failure(&self.0, name, io::Error::last_os_error()));
        }
        // SAFETY: freshly opened fd, owned by nothing else.
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }

    fn make_child_dir(&self, name: &OsStr) -> Result<(), GuardIo> {
        let c = cstr(name)?;
        // SAFETY: plain mkdirat(2) on a fd we own.
        if unsafe { libc::mkdirat(self.0.as_raw_fd(), c.as_ptr(), 0o755) } < 0 {
            return Err(GuardIo::last_os_error());
        }
        Ok(())
    }

    fn open_child_file(&self, name: &OsStr, mode: OpenMode) -> Result<File, GuardIo> {
        let flags = match mode {
            OpenMode::File => FILE_FLAGS,
            OpenMode::Dir => DIR_FLAGS,
        };
        let c = cstr(name)?;
        // SAFETY: `self.0` is an open directory fd we own; `c` is NUL-terminated.
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), flags) };
        if fd < 0 {
            return Err(classify_failure(&self.0, name, io::Error::last_os_error()));
        }
        // SAFETY: freshly opened fd, owned by nothing else.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn create_child_file(&self, name: &OsStr, excl: Excl) -> Result<File, GuardIo> {
        let extra = match excl {
            Excl::Truncate => libc::O_TRUNC,
            Excl::MustNotExist => libc::O_EXCL,
        };
        // `O_NOFOLLOW` on the leaf is what makes an existing symlink there fail
        // with `ELOOP` instead of writing through to its target.
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW | extra;
        let c = cstr(name)?;
        // SAFETY: `self.0` is an open directory fd we own; `c` is
        // NUL-terminated; `openat` is variadic and takes the mode as its third
        // argument when `O_CREAT` is set.
        let fd =
            unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), flags, 0o644 as libc::c_uint) };
        if fd < 0 {
            return Err(classify_failure(&self.0, name, io::Error::last_os_error()));
        }
        // SAFETY: freshly opened fd, owned by nothing else.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn rename_child(&self, from: &OsStr, to: &OsStr) -> Result<(), GuardIo> {
        let (old, new) = (cstr(from)?, cstr(to)?);
        // SAFETY: an open directory fd we own on both sides; both names are
        // NUL-terminated.
        if unsafe {
            libc::renameat(
                self.0.as_raw_fd(),
                old.as_ptr(),
                self.0.as_raw_fd(),
                new.as_ptr(),
            )
        } < 0
        {
            return Err(GuardIo::last_os_error());
        }
        Ok(())
    }

    fn unlink_child(&self, name: &OsStr) {
        let Ok(c) = cstr(name) else { return };
        // SAFETY: plain unlinkat(2) on a fd we own.
        unsafe {
            libc::unlinkat(self.0.as_raw_fd(), c.as_ptr(), 0);
        }
    }

    fn child_kind(&self, name: &OsStr) -> Option<NodeKind> {
        kind_at(&self.0, name)
    }

    fn child_attrs(&self, name: &OsStr) -> Option<Self::Attrs> {
        Some(stat_at(&self.0, name)?.st_mode)
    }

    fn apply_attrs(&self, file: &File, attrs: Self::Attrs) -> io::Result<()> {
        // SAFETY: plain fchmod(2) on a fd we own.
        if unsafe { libc::fchmod(file.as_raw_fd(), attrs & 0o7777) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn identity(&self) -> Result<NodeId, GuardIo> {
        use std::os::unix::fs::MetadataExt;
        let meta = self.0.metadata().map_err(GuardIo::io)?;
        Ok(NodeId {
            volume: meta.dev(),
            file: meta.ino() as u128,
        })
    }

    #[cfg(target_os = "linux")]
    fn resolve_beneath(&self, rel: &Path, mode: OpenMode) -> Result<Option<File>, GuardIo> {
        // Force layer 3 even where layer 2 exists, so CI exercises the portable
        // descent on Linux too. Without it the fallback — the layer that runs on
        // every pre-5.6 kernel and every seccomp-restricted container — would
        // only ever be tested on macOS runners, and would rot.
        if std::env::var("HOTL_FSGUARD_FORCE_DESCEND").is_ok_and(|v| !v.is_empty() && v != "0") {
            return Ok(None);
        }
        let c = CString::new(rel.as_os_str().as_bytes())
            .map_err(|_| GuardIo::io(io::Error::from(io::ErrorKind::InvalidInput)))?;
        let flags = match mode {
            OpenMode::File => FILE_FLAGS,
            OpenMode::Dir => DIR_FLAGS,
        };
        // `libc::open_how` is `#[non_exhaustive]`: it cannot be built with a
        // struct literal outside libc, so zero it and set the three fields.
        // SAFETY: `open_how` is a plain repr(C) struct of integers; all-zero is
        // a valid instance of it.
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (flags & !libc::O_NOFOLLOW) as u64;
        how.resolve =
            libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS | libc::RESOLVE_NO_SYMLINKS;
        // SAFETY: raw syscall with a valid dirfd, a NUL-terminated path, and a
        // correctly-sized `open_how` we own.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.0.as_raw_fd(),
                c.as_ptr(),
                &how as *const libc::open_how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if ret >= 0 {
            // SAFETY: the syscall returned a fresh fd owned by nothing else.
            return Ok(Some(unsafe {
                File::from_raw_fd(ret as std::os::unix::io::RawFd)
            }));
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // Pre-5.6 kernel, or a seccomp filter that blocks unknown syscalls.
            // Fall through to the portable descent, which is equally safe — it
            // just costs one syscall per component.
            Some(libc::ENOSYS) | Some(libc::EPERM) => Ok(None),
            Some(libc::ELOOP) | Some(libc::EXDEV) => Err(GuardIo::link(err)),
            _ => Err(GuardIo::io(err)),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn resolve_beneath(&self, _rel: &Path, _mode: OpenMode) -> Result<Option<File>, GuardIo> {
        // No `openat2` analogue outside Linux. `Ok(None)` is "this kernel does
        // not offer it", which is exactly true, and the caller descends.
        Ok(None)
    }

    fn into_file(self) -> File {
        self.0
    }

    fn sync_name_durability(&self) -> Result<(), GuardIo> {
        self.0.sync_all().map_err(GuardIo::io)
    }
}
