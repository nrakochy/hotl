//! Filesystem containment for the five file tools (`read`/`write`/`edit`/
//! `glob`/`grep`).
//!
//! INVARIANT: a path is contained iff a descent from the workspace-root file
//! descriptor reached it **without traversing a symlink**. The decision is
//! made on the fd the caller then uses, never on a name re-resolved after the
//! check — so there is no check/open window to lose. Enforced by
//! `symlinked_dir_is_lexically_inside_but_refused_by_the_guard`,
//! `symlinked_leaf_is_refused`, `symlinked_intermediate_component_is_refused`.
//!
//! This module deliberately replaces `builtins::workspace_contained`, whose
//! doc comment advertised pure-lexical checking as a strength ("can't be
//! defeated by a symlink race"). It could not be defeated by a *race*; it was
//! defeated by a plain `ln -s ~ link`.
//!
//! Three layers, in preference order:
//!
//! 1. Never follow symlinks during traversal (`ignore`/ripgrep default to it).
//! 2. Linux: `openat2` with `RESOLVE_BENEATH` — one syscall, one resolution.
//! 3. Portable: component-wise `openat` from the root fd, every component
//!    `O_NOFOLLOW`, then `fstat` on the fd you hold.
//!
//! hotl is unix-only today (`libc::kill` in `builtins.rs`, `process_group(0)`,
//! Landlock/Seatbelt in `sandbox.rs`); `fsguard` follows the same posture. A
//! Windows port is reserved in the tech-debt tracker.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// The process workspace root, canonicalized once. Also the default root for
/// a session that was not given one of its own.
///
/// INVARIANT: the guard's root set — a session's root, plus any
/// `[sandbox].writable` extras a tool was granted via `[sandbox].file_tools`
/// — is a subset of the kernel write set (`sandbox::write_roots()`).
/// A session root is either this directory or one **beneath** it (an isolated
/// child's git worktree, `hotl::agent::HotlChildBuilder::spawn_child`), and the
/// floor covers the whole workspace, so the containment holds either way. The
/// equal case holds because `sandbox.rs` (seatbelt and landlock alike)
/// canonicalizes `current_dir()` exactly like this function — enforced by
/// `guard_root_matches_sandbox_root`; the beneath case by
/// `child_root_is_beneath_the_sandbox_root`. The extras half holds by
/// construction — both sides read the same set-once `sandbox::EXTRAS`. If
/// either boundary named a directory the other does not, one of them would be
/// decorative.
pub(crate) fn workspace_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        std::env::current_dir()
            .and_then(|p| p.canonicalize())
            .unwrap_or_else(|_| PathBuf::from("."))
    })
}

/// Which granted extra root (if any) an **absolute** path lands under, and
/// where under it. Purely lexical — `.`/`..` are normalized first, and the
/// caller then descends from the (canonical) root fd with `O_NOFOLLOW`, so
/// the normalized name is the object actually opened. Nested roots: the
/// deepest match wins, so `rel` is computed against the root the descent
/// starts from. The root itself maps to `.` (which no write path accepts —
/// `split_parent` refuses a bare `.`).
pub(crate) fn extra_root_for(path: &str, extras: &[PathBuf]) -> Option<(PathBuf, PathBuf)> {
    let given = Path::new(path);
    if !given.is_absolute() {
        return None;
    }
    let mut norm = PathBuf::new();
    for c in given.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => norm.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                if !norm.pop() {
                    return None;
                }
            }
            Component::Normal(s) => norm.push(s),
        }
    }
    let root = extras
        .iter()
        .filter(|root| norm.starts_with(root))
        .max_by_key(|root| root.components().count())?;
    let rel = norm.strip_prefix(root).ok()?;
    Some((
        root.clone(),
        if rel.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            rel.to_path_buf()
        },
    ))
}

/// Where a model-supplied path lands, **by lexical inspection alone**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Placement {
    /// Root-relative and normalized; no `..` remains.
    Inside(PathBuf),
    /// Absolute, or escapes above the root. Carries the path as given, for
    /// the ask summary.
    Outside(PathBuf),
}

/// Cheap pre-filter: normalize `.`/`..` and decide which *permission* to ask
/// for.
///
/// INVARIANT (this is NOT the security boundary): `classify` is string-only.
/// It exists to produce good error messages and to pick the permission
/// variant before anything is opened. A path it calls `Inside` may still
/// escape through a symlink — that is `open_beneath`'s job to refuse, and it
/// does, on the fd. Enforced by
/// `symlinked_dir_is_lexically_inside_but_refused_by_the_guard`.
pub(crate) fn classify(_root: &Path, path: &str) -> Placement {
    let given = PathBuf::from(path);
    if given.is_absolute() {
        return Placement::Outside(given);
    }
    let mut out: Vec<&OsStr> = Vec::new();
    for c in given.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.pop().is_none() {
                    return Placement::Outside(given);
                }
            }
            Component::Normal(s) => out.push(s),
            Component::RootDir | Component::Prefix(_) => return Placement::Outside(given),
        }
    }
    let mut rel = PathBuf::new();
    for s in out {
        rel.push(s);
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    Placement::Inside(rel)
}

#[derive(Debug)]
pub(crate) enum GuardError {
    /// A symlink (or a `..`) stood between the root and the target. `at` is
    /// the root-relative component where the descent stopped.
    Escape {
        at: PathBuf,
    },
    /// Opened, but not a regular file (FIFO, device, socket) — reading it
    /// could block the turn forever.
    NotRegular,
    Io(std::io::Error),
}

impl GuardError {
    /// Errors are prompts: tell the model exactly what to do next.
    pub(crate) fn prompt(&self, tool: &str, path: &str) -> String {
        match self {
            GuardError::Escape { at } => format!(
                "`{path}` leaves the working directory: `{}` is a symlink (or a `..` escape), and \
                 `{tool}` never follows one. If you really mean the file it points at, re-issue \
                 the call with that file's absolute path — it will be gated with an explicit ask.",
                at.display()
            ),
            GuardError::NotRegular => format!(
                "`{path}` is not a regular file (it is a FIFO, device, or socket); reading it \
                 could block forever. Pick a real file."
            ),
            GuardError::Io(e) => {
                format!("Could not open `{path}`: {e}. Check the path with `glob` and try again.")
            }
        }
    }
}

/// Read flags for a guarded open.
///
/// `O_NONBLOCK` is not decoration: `open(2)` on a FIFO blocks until a writer
/// appears, so without it the guard would hang *before* `fstat` ever got the
/// chance to refuse the FIFO. It is cleared again once the fd is known to be
/// a regular file. Enforced by `a_fifo_is_not_a_readable_file`.
const OPEN_RD: libc::c_int = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;

/// Open `rel` (root-relative, `..`-free) for reading with no symlink
/// traversal. Linux takes one atomic `openat2`; everything else descends.
pub(crate) fn open_beneath(root: &Path, rel: &Path) -> Result<File, GuardError> {
    let file = open_leaf(root, rel, OPEN_RD)?;
    let meta = file.metadata().map_err(GuardError::Io)?;
    if !meta.is_file() {
        return Err(GuardError::NotRegular);
    }
    clear_nonblock(&file)?;
    Ok(file)
}

/// Same, for a directory (`O_DIRECTORY`). `glob` uses this on its search root:
/// a walk root that is not a directory is a mistake worth naming, and one
/// reached through a symlink is an escape.
pub(crate) fn open_dir_beneath(root: &Path, rel: &Path) -> Result<File, GuardError> {
    open_leaf(root, rel, OPEN_RD | libc::O_DIRECTORY)
}

/// Confirm `rel` is beneath `root` and hand back the joined path.
///
/// INVARIANT: after a successful `O_NOFOLLOW` descent, `root.join(rel)` and
/// the kernel's own resolution name the same object — no component was a
/// link, so the lexical join *is* the real path. That is what makes it safe
/// to hand the joined path to an external walker (`ignore`, `rg`) that takes
/// a path rather than an fd. Enforced by `resolved_path_is_the_same_object`.
pub(crate) fn resolve_beneath(root: &Path, rel: &Path) -> Result<PathBuf, GuardError> {
    let fd = open_leaf(root, rel, OPEN_RD)?;
    let joined = join_beneath(root, rel);
    // Belt-and-braces: the fd we validated and the path we return are the
    // same inode on the same device.
    let by_fd = fd.metadata().map_err(GuardError::Io)?;
    let by_name = std::fs::symlink_metadata(&joined).map_err(GuardError::Io)?;
    use std::os::unix::fs::MetadataExt;
    if by_fd.dev() != by_name.dev() || by_fd.ino() != by_name.ino() {
        return Err(GuardError::Escape {
            at: rel.to_path_buf(),
        });
    }
    Ok(joined)
}

/// Create (or truncate) `rel` beneath `root`. The parent is reached by the
/// same `O_NOFOLLOW` descent; the leaf is created with `O_NOFOLLOW` so an
/// existing symlink there fails with `ELOOP` instead of writing its target.
///
/// INVARIANT: a write never follows a symlink, at any component, including
/// the final one. Enforced by
/// `write_through_a_symlink_does_not_touch_the_target` and
/// `write_creates_parents_but_never_through_a_link`.
pub(crate) fn create_beneath(root: &Path, rel: &Path, mkparents: bool) -> Result<File, GuardError> {
    let (dir, leaf) = split_parent(root, rel, mkparents)?;
    openat_leaf(
        &dir,
        &leaf,
        libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        0o644,
    )
}

/// Atomically replace `rel`'s contents: write a sibling temp file, `fsync`
/// it, `renameat` over the target, then `fsync` the parent directory.
///
/// INVARIANT: a crash mid-write leaves either the old file or the new one,
/// never a truncated one; the target's mode survives the swap; and a symlink
/// standing where the target should be is refused rather than followed *or*
/// silently replaced (`renameat` does not follow links, so without the check
/// an atomic replace would quietly delete the link). Enforced by
/// `replace_is_atomic_and_durable` and
/// `edit_inside_the_tree_still_works_and_keeps_the_mode`.
pub(crate) fn replace_beneath(root: &Path, rel: &Path, bytes: &[u8]) -> Result<(), GuardError> {
    use std::io::Write;
    let (dir, leaf) = split_parent(root, rel, false)?;
    // What is standing there now, if anything — checked before a byte is
    // staged, so a refusal leaves no debris.
    let existing = stat_mode_at(&dir, &leaf);
    if let Some(mode) = existing {
        match mode & libc::S_IFMT {
            libc::S_IFREG => {}
            libc::S_IFLNK => {
                return Err(GuardError::Escape {
                    at: rel.to_path_buf(),
                })
            }
            _ => return Err(GuardError::NotRegular),
        }
    }
    let tmp = std::ffi::OsString::from(format!(
        ".hotl-tmp-{}-{}",
        std::process::id(),
        next_tmp_seq()
    ));
    let mut f = openat_leaf(
        &dir,
        &tmp,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        0o644,
    )?;
    // Carry the target's mode across, or an atomic replace would silently
    // drop the executable bit off every script it edits.
    let staged = (|| {
        if let Some(mode) = existing {
            fchmod(&f, mode & 0o7777)?;
        }
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = staged {
        unlinkat(&dir, &tmp);
        return Err(GuardError::Io(e));
    }
    drop(f);
    if let Err(e) = renameat(&dir, &tmp, &dir, &leaf) {
        unlinkat(&dir, &tmp);
        return Err(GuardError::Io(e));
    }
    // Durability of the *name*: without this the rename can be lost even
    // though the data was synced.
    dir.sync_all().map_err(GuardError::Io)
}

/// Descend to `rel`'s parent and hand back `(parent dir fd, leaf name)`.
/// Missing intermediates are created with `mkdirat` when `mkparents`, and
/// every one that already exists is re-opened `O_NOFOLLOW|O_DIRECTORY`, so a
/// symlinked intermediate is refused rather than descended through.
fn split_parent(
    root: &Path,
    rel: &Path,
    mkparents: bool,
) -> Result<(File, std::ffi::OsString), GuardError> {
    let comps: Vec<&OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::CurDir => None,
            Component::Normal(s) => Some(Ok(s)),
            _ => Some(Err(())),
        })
        .collect::<Result<Vec<_>, ()>>()
        .map_err(|_| GuardError::Escape {
            at: rel.to_path_buf(),
        })?;
    // A bare `.` names the root directory, which is not a file to write.
    let Some((leaf, parents)) = comps.split_last() else {
        return Err(GuardError::Escape {
            at: rel.to_path_buf(),
        });
    };
    let mut dir = open_root(root)?;
    let mut walked = PathBuf::new();
    for comp in parents {
        walked.push(comp);
        dir = open_or_make_dir(&dir, comp, mkparents, &walked)?;
    }
    Ok((dir, (*leaf).to_os_string()))
}

fn open_or_make_dir(
    dir: &File,
    comp: &OsStr,
    mkparents: bool,
    walked: &Path,
) -> Result<File, GuardError> {
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY;
    let c = CString::new(comp.as_bytes()).map_err(|_| GuardError::Escape {
        at: walked.to_path_buf(),
    })?;
    // SAFETY: `dir` is an open directory fd we own; `c` is NUL-terminated.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
    if fd >= 0 {
        // SAFETY: freshly opened fd, owned by nothing else.
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    let err = std::io::Error::last_os_error();
    if mkparents && err.raw_os_error() == Some(libc::ENOENT) {
        // SAFETY: plain mkdirat(2) on a fd we own.
        if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o755) } < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(GuardError::Io(e));
            }
        }
        // SAFETY: same, re-opening what we just created.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
        if fd >= 0 {
            // SAFETY: freshly opened fd, owned by nothing else.
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        return Err(GuardError::Io(std::io::Error::last_os_error()));
    }
    // Same classification as `descend`: a link is an escape, anything else is
    // an ordinary I/O error.
    Err(
        if matches!(err.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
            && is_symlink_at(dir, comp)
        {
            GuardError::Escape {
                at: walked.to_path_buf(),
            }
        } else {
            GuardError::Io(err)
        },
    )
}

fn openat_leaf(
    dir: &File,
    leaf: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<File, GuardError> {
    let c = CString::new(leaf.as_bytes()).map_err(|_| GuardError::Escape {
        at: PathBuf::from(leaf),
    })?;
    // SAFETY: `dir` is an open directory fd we own; `c` is NUL-terminated;
    // `openat` is variadic and takes the mode as its third argument when
    // `O_CREAT` is set.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, mode as libc::c_uint) };
    if fd >= 0 {
        // SAFETY: freshly opened fd, owned by nothing else.
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    let err = std::io::Error::last_os_error();
    Err(
        if matches!(err.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
            && is_symlink_at(dir, leaf)
        {
            GuardError::Escape {
                at: PathBuf::from(leaf),
            }
        } else {
            GuardError::Io(err)
        },
    )
}

/// The full `st_mode` of `name` in `dir` without following a final symlink,
/// or `None` if it does not exist.
fn stat_mode_at(dir: &File, name: &OsStr) -> Option<libc::mode_t> {
    let c = CString::new(name.as_bytes()).ok()?;
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
    if rc != 0 {
        return None;
    }
    Some(st.st_mode)
}

fn fchmod(f: &File, mode: libc::mode_t) -> std::io::Result<()> {
    // SAFETY: plain fchmod(2) on a fd we own.
    if unsafe { libc::fchmod(f.as_raw_fd(), mode) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn unlinkat(dir: &File, name: &OsStr) {
    let Ok(c) = CString::new(name.as_bytes()) else {
        return;
    };
    // SAFETY: plain unlinkat(2) on a fd we own. Best-effort cleanup of our
    // own temp file; a failure here has nothing to report.
    unsafe {
        libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0);
    }
}

fn renameat(olddir: &File, oldname: &OsStr, newdir: &File, newname: &OsStr) -> std::io::Result<()> {
    let old = CString::new(oldname.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let new = CString::new(newname.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: both fds are open directories we own; both names are
    // NUL-terminated.
    if unsafe {
        libc::renameat(
            olddir.as_raw_fd(),
            old.as_ptr(),
            newdir.as_raw_fd(),
            new.as_ptr(),
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Per-process counter so two concurrent replaces cannot pick the same temp
/// name (the pid alone is not enough).
fn next_tmp_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// `root.join(rel)` with the lone `.` collapsed, so a caller handing the
/// result to an external walker gets `root` rather than `root/.`.
pub(crate) fn join_beneath(root: &Path, rel: &Path) -> PathBuf {
    if rel.as_os_str() == "." {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

fn open_leaf(root: &Path, rel: &Path, flags: libc::c_int) -> Result<File, GuardError> {
    let rootfd = open_root(root)?;
    #[cfg(target_os = "linux")]
    if !force_descend() {
        if let Some(f) = openat2_beneath(rootfd.as_raw_fd(), rel, flags)? {
            return Ok(f);
        }
    }
    descend(rootfd, rel, flags)
}

fn open_root(root: &Path) -> Result<File, GuardError> {
    let c = CString::new(root.as_os_str().as_bytes()).map_err(|_| GuardError::Escape {
        at: root.to_path_buf(),
    })?;
    // The root itself is opened by name (it is ours, not the model's) but
    // with O_DIRECTORY so a swapped-out root fails loudly rather than
    // silently rebasing the whole guard.
    // SAFETY: plain open(2) on a NUL-terminated path we own.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        return Err(GuardError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: `fd` was just opened and is owned by nothing else.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Innovation #1: force layer 3 even where layer 2 exists, so CI exercises the
/// portable descent on Linux too. Without it the fallback — the layer that runs
/// on every pre-5.6 kernel and every seccomp-restricted container — would only
/// ever be tested on macOS runners, and would rot.
#[cfg(target_os = "linux")]
fn force_descend() -> bool {
    static FORCE: OnceLock<bool> = OnceLock::new();
    *FORCE.get_or_init(|| {
        std::env::var("HOTL_FSGUARD_FORCE_DESCEND").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// Layer 2 — Linux >= 5.6. One syscall, one resolution, kernel-enforced.
/// `RESOLVE_BENEATH` refuses any escape above `dirfd`; `RESOLVE_NO_SYMLINKS`
/// refuses every link, matching layer 3's rule exactly;
/// `RESOLVE_NO_MAGICLINKS` refuses `/proc/*/fd` style re-entry.
#[cfg(target_os = "linux")]
fn openat2_beneath(
    dirfd: RawFd,
    rel: &Path,
    flags: libc::c_int,
) -> Result<Option<File>, GuardError> {
    let c = CString::new(rel.as_os_str().as_bytes()).map_err(|_| GuardError::Escape {
        at: rel.to_path_buf(),
    })?;
    // `libc::open_how` is `#[non_exhaustive]`: it cannot be built with a
    // struct literal outside libc, so zero it and set the three fields.
    // SAFETY: `open_how` is a plain repr(C) struct of integers; all-zero is a
    // valid instance of it.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (flags & !libc::O_NOFOLLOW) as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS | libc::RESOLVE_NO_SYMLINKS;
    // SAFETY: raw syscall with a valid dirfd, a NUL-terminated path, and a
    // correctly-sized `open_how` we own.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            c.as_ptr(),
            &how as *const libc::open_how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if ret >= 0 {
        // SAFETY: the syscall returned a fresh fd owned by nothing else.
        return Ok(Some(unsafe { File::from_raw_fd(ret as RawFd) }));
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // Pre-5.6 kernel, or a seccomp filter that blocks unknown syscalls.
        // Fall through to the portable descent, which is equally safe — it
        // just costs one syscall per component.
        Some(libc::ENOSYS) | Some(libc::EPERM) => Ok(None),
        Some(libc::ELOOP) | Some(libc::EXDEV) => Err(GuardError::Escape {
            at: rel.to_path_buf(),
        }),
        _ => Err(GuardError::Io(err)),
    }
}

/// Layer 3 — portable. Walk the components from the root fd, refusing a
/// symlink at every step. `..` never reaches here (`classify` strips it) and
/// is refused again defensively: rejecting it on the *component stream* is
/// sound because there is no second resolution to disagree with.
fn descend(root: File, rel: &Path, leaf_flags: libc::c_int) -> Result<File, GuardError> {
    let mut dir = root;
    let comps: Vec<&OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::CurDir => None,
            Component::Normal(s) => Some(Ok(s)),
            _ => Some(Err(())),
        })
        .collect::<Result<Vec<_>, ()>>()
        .map_err(|_| GuardError::Escape {
            at: rel.to_path_buf(),
        })?;
    if comps.is_empty() {
        return Ok(dir); // "." — the root fd itself
    }
    let last = comps.len() - 1;
    let mut walked = PathBuf::new();
    for (i, comp) in comps.into_iter().enumerate() {
        walked.push(comp);
        let flags = if i == last {
            leaf_flags | libc::O_NOFOLLOW
        } else {
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY
        };
        let c =
            CString::new(comp.as_bytes()).map_err(|_| GuardError::Escape { at: walked.clone() })?;
        // SAFETY: `dir` is an open directory fd we own; `c` is NUL-terminated.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            // POSIX says `O_NOFOLLOW` on a symlink is ELOOP, but macOS returns
            // ENOTDIR whenever `O_DIRECTORY` is also set, and an intermediate
            // link can surface either. So classify by asking the *same dirfd*
            // whether the component is a link. The open has already failed —
            // this only picks the error message, and nothing is ever opened on
            // its strength, so it is not a check-then-open.
            let refused_a_link =
                matches!(err.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
                    && is_symlink_at(&dir, comp);
            return Err(if refused_a_link {
                GuardError::Escape { at: walked }
            } else {
                GuardError::Io(err)
            });
        }
        // SAFETY: `fd` was just opened and is owned by nothing else.
        dir = unsafe { File::from_raw_fd(fd) };
    }
    Ok(dir)
}

/// Is `comp` a symlink, relative to the directory `dir` names? Used only to
/// classify an `openat` that already failed (see `descend`).
fn is_symlink_at(dir: &File, comp: &OsStr) -> bool {
    let Ok(c) = CString::new(comp.as_bytes()) else {
        return false;
    };
    // SAFETY: `stat` is a plain repr(C) struct; all-zero is a valid instance.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is an open directory fd we own; `c` is NUL-terminated;
    // `st` is a live, correctly-typed out-parameter.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    rc == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK
}

/// Drop the `O_NONBLOCK` that `OPEN_RD` needed to survive a FIFO. On a regular
/// file the flag is inert, but leaving it set would leak an odd fd mode into
/// `tokio::fs::File`.
fn clear_nonblock(f: &File) -> Result<(), GuardError> {
    // SAFETY: plain fcntl(2) on a fd we own.
    let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(GuardError::Io(std::io::Error::last_os_error()));
    }
    if flags & libc::O_NONBLOCK == 0 {
        return Ok(());
    }
    // SAFETY: same fd, clearing one flag we set ourselves.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(GuardError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A workspace with an out-of-tree "home" beside it.
    pub(crate) fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("workspace");
        let home = outer.path().join("home");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("id_rsa"), "BEGIN PRIVATE KEY\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\n").unwrap();
        // canonicalize: on macOS the tempdir is under a /var -> /private/var link.
        (
            outer,
            root.canonicalize().unwrap(),
            home.canonicalize().unwrap(),
        )
    }

    #[test]
    fn lexical_classification_is_only_a_prefilter() {
        let (_o, root, _home) = fixture();
        // These are the tier1 helper's cases and they still hold...
        assert!(matches!(classify(&root, "src"), Placement::Inside(_)));
        assert!(matches!(
            classify(&root, "./src/lib.rs"),
            Placement::Inside(_)
        ));
        assert!(matches!(
            classify(&root, "src/../README.md"),
            Placement::Inside(_)
        ));
        assert!(matches!(
            classify(&root, "/etc/passwd"),
            Placement::Outside(_)
        ));
        assert!(matches!(
            classify(&root, "../secrets"),
            Placement::Outside(_)
        ));
        assert!(matches!(
            classify(&root, "src/../../etc"),
            Placement::Outside(_)
        ));
    }

    #[test]
    fn symlinked_dir_is_lexically_inside_but_refused_by_the_guard() {
        let (_o, root, home) = fixture();
        symlink(&home, root.join("link")).unwrap();
        // The tier1 lexical check passes it — this is the bug, reproduced.
        assert!(matches!(classify(&root, "link"), Placement::Inside(_)));
        // The fd check refuses it. No race, no timing, no retry.
        let err = open_dir_beneath(&root, std::path::Path::new("link")).unwrap_err();
        assert!(matches!(err, GuardError::Escape { .. }), "{err:?}");
    }

    #[test]
    fn symlinked_leaf_is_refused() {
        let (_o, root, home) = fixture();
        symlink(home.join("id_rsa"), root.join("notes.md")).unwrap();
        let err = open_beneath(&root, std::path::Path::new("notes.md")).unwrap_err();
        assert!(matches!(err, GuardError::Escape { .. }), "{err:?}");
    }

    #[test]
    fn symlinked_intermediate_component_is_refused() {
        let (_o, root, home) = fixture();
        std::fs::write(home.join("secret.txt"), "s").unwrap();
        symlink(&home, root.join("src/out")).unwrap();
        let err = open_beneath(&root, std::path::Path::new("src/out/secret.txt")).unwrap_err();
        assert!(matches!(err, GuardError::Escape { .. }), "{err:?}");
    }

    #[test]
    fn a_symlink_that_stays_inside_is_still_refused_and_says_so() {
        // Deliberate: we do not distinguish "symlink pointing inside" from
        // "symlink pointing outside". Resolving-then-comparing would reintroduce
        // a name-based check; refusing every symlink keeps the rule "the fd chain
        // never traversed a link", which is checkable with no second resolution.
        let (_o, root, _home) = fixture();
        symlink(root.join("src/lib.rs"), root.join("alias.rs")).unwrap();
        let err = open_beneath(&root, std::path::Path::new("alias.rs")).unwrap_err();
        assert!(matches!(err, GuardError::Escape { .. }), "{err:?}");
        // The refusal is a prompt: it names the offending component and says
        // what to do instead.
        let prompt = err.prompt("read", "alias.rs");
        assert!(
            prompt.contains("alias.rs") && prompt.contains("absolute path"),
            "{prompt}"
        );
    }

    #[test]
    fn ordinary_files_open_and_hardlinks_are_fine() {
        let (_o, root, _home) = fixture();
        let f = open_beneath(&root, std::path::Path::new("src/lib.rs")).unwrap();
        assert!(f.metadata().unwrap().is_file());
        assert!(resolve_beneath(&root, std::path::Path::new("src")).is_ok());
        // "." resolves to the root itself.
        assert!(open_dir_beneath(&root, std::path::Path::new(".")).is_ok());
        // A hard link is a second name for the same inode, not a traversal —
        // nothing was followed, so it opens.
        std::fs::hard_link(root.join("src/lib.rs"), root.join("hard.rs")).unwrap();
        assert!(open_beneath(&root, std::path::Path::new("hard.rs")).is_ok());
    }

    #[test]
    fn a_fifo_is_not_a_readable_file() {
        // A hostile (or merely unusual) repo containing a FIFO must not hang the
        // turn: we fstat the fd we hold and require a regular file.
        let (_o, root, _home) = fixture();
        let p = root.join("pipe");
        let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        // SAFETY: plain mkfifo(3) on a path inside our own tempdir.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        assert!(matches!(
            open_beneath(&root, std::path::Path::new("pipe")),
            Err(GuardError::NotRegular)
        ));
    }

    fn tmp_siblings(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".hotl-tmp-"))
            .collect()
    }

    #[test]
    fn replace_is_atomic_and_durable() {
        let (_o, root, home) = fixture();
        let rel = std::path::Path::new("src/lib.rs");
        replace_beneath(&root, rel, b"fn b() {}\n").unwrap();
        // Fully the new bytes — never a truncated prefix of them.
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "fn b() {}\n"
        );
        assert!(
            tmp_siblings(&root.join("src")).is_empty(),
            "a temp sibling survived a successful replace"
        );

        // A symlinked leaf is refused before anything is staged: the link is
        // neither followed (the target keeps its bytes) nor clobbered (the
        // link is still a link), and no debris is left behind.
        symlink(home.join("id_rsa"), root.join("notes.md")).unwrap();
        assert!(matches!(
            replace_beneath(&root, std::path::Path::new("notes.md"), b"evil"),
            Err(GuardError::Escape { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(home.join("id_rsa")).unwrap(),
            "BEGIN PRIVATE KEY\n"
        );
        assert!(std::fs::symlink_metadata(root.join("notes.md"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(tmp_siblings(&root).is_empty(), "debris after a refusal");
    }

    #[test]
    fn guard_root_matches_sandbox_root() {
        // `sandbox.rs:180`/`:270` both compute `canon(current_dir())` for the
        // kernel write-confinement floor. If the tool boundary named a
        // different directory, one of the two would be decorative.
        let sandbox_root = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(workspace_root(), sandbox_root.as_path());
    }

    #[test]
    fn child_root_is_beneath_the_sandbox_root() {
        // The weakened half of the INVARIANT above: an isolated child roots its
        // tools at `<workspace>/.git/hotl-worktrees/<id>`, which is not equal to
        // the kernel floor's cwd entry but is contained by it. If this ever
        // stopped holding — a worktree in `$TMPDIR`, say — the file tools would
        // happily descend into a directory the kernel then refuses to write.
        let floor = crate::sandbox::write_roots()
            .first()
            .expect("the write floor always carries the workspace")
            .clone();
        let child = floor.join(".git/hotl-worktrees/01JEXAMPLE");
        assert!(
            child.starts_with(&floor),
            "child root {} escaped the floor {}",
            child.display(),
            floor.display()
        );
    }

    #[test]
    fn extra_root_for_matches_absolute_paths_by_longest_prefix() {
        let outer = PathBuf::from("/srv/cache");
        let inner = PathBuf::from("/srv/cache/deep");
        let extras = vec![outer.clone(), inner.clone()];

        // Under one root: (root, rel).
        let (root, rel) = extra_root_for("/srv/cache/a/b.txt", &extras).unwrap();
        assert_eq!((root, rel), (outer.clone(), PathBuf::from("a/b.txt")));
        // Nested roots: the deepest match wins, so `rel` is right for it.
        let (root, rel) = extra_root_for("/srv/cache/deep/x", &extras).unwrap();
        assert_eq!((root, rel), (inner.clone(), PathBuf::from("x")));
        // The root itself: `.` (a later write to `.` is refused downstream).
        let (root, rel) = extra_root_for("/srv/cache/deep", &extras).unwrap();
        assert_eq!((root, rel), (inner, PathBuf::from(".")));
        // Lexical `.`/`..` are normalized before matching (the descent then
        // runs from the root fd, so the normalized name is the one opened —
        // a `..` detour cannot smuggle a different object in).
        let (_, rel) = extra_root_for("/srv/cache/./a/../b", &extras).unwrap();
        assert_eq!(rel, PathBuf::from("b"));
        let (_, rel) = extra_root_for("/srv/cache/../cache/x", &extras).unwrap();
        assert_eq!(rel, PathBuf::from("x"));
        // A normalization that lands outside stays outside.
        assert!(extra_root_for("/srv/cache/../other/x", &extras).is_none());
        // Outside every root, relative paths, and prefix-of-a-component
        // near-misses all miss.
        assert!(extra_root_for("/srv/other/x", &extras).is_none());
        assert!(extra_root_for("cache/x", &extras).is_none());
        assert!(extra_root_for("/srv/cachette/x", &extras).is_none());
        assert!(extra_root_for("/srv/cache/a", &[]).is_none());
    }

    #[test]
    fn resolved_path_is_the_same_object() {
        let (_o, root, home) = fixture();
        let joined = resolve_beneath(&root, std::path::Path::new("src/lib.rs")).unwrap();
        assert_eq!(joined, root.join("src/lib.rs"));
        // Same inode by name and by the fd the guard validated.
        use std::os::unix::fs::MetadataExt;
        let by_name = std::fs::symlink_metadata(&joined).unwrap();
        let by_fd = open_beneath(&root, std::path::Path::new("src/lib.rs"))
            .unwrap()
            .metadata()
            .unwrap();
        assert_eq!((by_name.dev(), by_name.ino()), (by_fd.dev(), by_fd.ino()));
        // `.` resolves to the root itself, not `root/.`.
        assert_eq!(
            resolve_beneath(&root, std::path::Path::new(".")).unwrap(),
            root
        );
        // And a symlinked component never resolves at all.
        symlink(&home, root.join("link")).unwrap();
        assert!(matches!(
            resolve_beneath(&root, std::path::Path::new("link/id_rsa")),
            Err(GuardError::Escape { .. })
        ));
    }
}
