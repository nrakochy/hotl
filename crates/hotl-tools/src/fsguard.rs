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
//! The syscall layer lives behind `hotl_platform::DirHandle`, so the descent
//! above is written once and compiled for every platform. That trait is
//! deliberately narrow: **no method on it accepts an absolute or
//! multi-component path**, so code handed a handle cannot reach outside it. The
//! one-door property is enforced by the type rather than by review, which is
//! stronger than the free functions this module used when it had one platform.

use std::ffi::OsStr;
use std::fs::File;
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
            .and_then(dunce::canonicalize)
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
    /// A component the filesystem and hotl's own matchers would disagree
    /// about: an alternate data stream, a trailing dot or space, a reserved
    /// device name. Refused before any syscall — see
    /// `hotl_platform::openat::refuse_component`.
    BadName {
        at: PathBuf,
        why: &'static str,
    },
    /// The descent succeeded, but the OS's normalized name for the handle is
    /// not the path we were about to hand out — an 8.3 short name, or a
    /// verbatim form too long to shorten. Fail closed rather than pass on a
    /// name that means something else to the next matcher.
    Unnormalized {
        at: PathBuf,
    },
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
            GuardError::BadName { at, why } => format!(
                "`{path}` cannot be used: the component `{}` {why}. Windows resolves names like \
                 that to a different file than the one written, so hotl refuses them on every \
                 platform. Rename it and try again.",
                at.display()
            ),
            GuardError::Unnormalized { at } => format!(
                "`{path}` resolved to a different name than `{}` — a short name, or a path too \
                 long to express plainly. Re-issue the call with the name the filesystem reports.",
                at.display()
            ),
            GuardError::Io(e) => {
                format!("Could not open `{path}`: {e}. Check the path with `glob` and try again.")
            }
        }
    }
}

/// The active syscall backend. Bound in exactly one place, so the descent
/// algorithm below is written once and compiled for every platform — and so a
/// future port implements a trait rather than editing this file.
type Dir = hotl_platform::ActiveDirHandle;

use hotl_platform::{DirHandle as _, Excl, GuardIo, NodeKind, OpenMode};

/// A failed syscall, at a known point in the descent.
///
/// The adapter already decided *whether* the OS refused because of a link (it
/// alone knows its own errno quirks); this decides what that means, which is
/// policy and belongs here.
fn at(e: GuardIo, walked: &Path) -> GuardError {
    if e.refused_a_link {
        GuardError::Escape {
            at: walked.to_path_buf(),
        }
    } else {
        GuardError::Io(e.error)
    }
}

/// `rel`'s components, refusing anything that is not a plain name.
///
/// Two refusals, both fail-closed. `..` and a root never reach here (`classify`
/// strips them) and are rejected again defensively — sound on the *component
/// stream*, because there is no second resolution to disagree with. And every
/// component runs the lexical filter, so an alternate data stream, a trailing
/// dot, or a reserved device name is refused before the OS ever sees it.
fn components_of(rel: &Path) -> Result<Vec<&OsStr>, GuardError> {
    let mut out = Vec::new();
    for c in rel.components() {
        match c {
            Component::CurDir => {}
            Component::Normal(s) => {
                if let Some(why) = hotl_platform::openat::refuse_component(s) {
                    return Err(GuardError::BadName {
                        at: PathBuf::from(s),
                        why,
                    });
                }
                out.push(s);
            }
            _ => {
                return Err(GuardError::Escape {
                    at: rel.to_path_buf(),
                })
            }
        }
    }
    Ok(out)
}

/// Open `rel` (root-relative, `..`-free) for reading with no symlink
/// traversal. Layer 2 first where the kernel has it, then the descent.
pub(crate) fn open_beneath(root: &Path, rel: &Path) -> Result<File, GuardError> {
    let file = open_leaf(root, rel, OpenMode::File)?;
    let meta = file.metadata().map_err(GuardError::Io)?;
    if !meta.is_file() {
        return Err(GuardError::NotRegular);
    }
    hotl_platform::openat::unblock(&file).map_err(GuardError::Io)?;
    Ok(file)
}

/// Same, for a directory. `glob` uses this on its search root: a walk root that
/// is not a directory is a mistake worth naming, and one reached through a
/// symlink is an escape.
pub(crate) fn open_dir_beneath(root: &Path, rel: &Path) -> Result<File, GuardError> {
    open_leaf(root, rel, OpenMode::Dir)
}

/// Confirm `rel` is beneath `root` and hand back the joined path.
///
/// INVARIANT: after a successful no-follow descent, `root.join(rel)` and the
/// kernel's own resolution name the same object — no component was a link, so
/// the lexical join *is* the real path. That is what makes it safe to hand the
/// joined path to an external walker (`ignore`, `rg`) that takes a path rather
/// than a handle. Enforced by `resolved_path_is_the_same_object`.
pub(crate) fn resolve_beneath(root: &Path, rel: &Path) -> Result<PathBuf, GuardError> {
    // A directory cannot be opened in File mode on Windows (unix tolerates it),
    // and resolve_beneath must handle both — glob's search root is a directory.
    let handle = match open_leaf(root, rel, OpenMode::File) {
        Ok(h) => h,
        Err(_) => open_leaf(root, rel, OpenMode::Dir)?,
    };
    let joined = join_beneath(root, rel);
    // Belt-and-braces: the handle we validated and the path we return are the
    // same object on the same volume.
    let by_handle = hotl_platform::openat::identity_of(&handle).map_err(GuardError::Io)?;
    let by_name = hotl_platform::openat::identity_at(&joined).map_err(GuardError::Io)?;
    if by_handle != by_name {
        return Err(GuardError::Escape {
            at: rel.to_path_buf(),
        });
    }
    // Where the OS will tell us its own normalized name for the handle, require
    // it to agree with the path we are about to hand out. This is what catches
    // 8.3 short names, case folding, and trailing dot/space forms in one call —
    // all cases where a matcher and the filesystem would name different files.
    // Living in the shared algorithm rather than an adapter means Unix gets the
    // check for free the day a Unix answer exists.
    if let Some(normalized) =
        hotl_platform::openat::normalized_name(&handle).map_err(GuardError::Io)?
    {
        let expected = dunce::canonicalize(&joined).map_err(GuardError::Io)?;
        // `GetFinalPathNameByHandle` hands back a `\\?\`-verbatim path; strip it
        // to match `dunce::canonicalize`'s de-verbatim'd form, or every Windows
        // path mismatches on the prefix alone. (A >260-char path dunce cannot
        // shorten stays verbatim on both sides, preserving the fail-closed case.)
        if !same_path(dunce::simplified(&normalized), &expected) {
            return Err(GuardError::Unnormalized {
                at: rel.to_path_buf(),
            });
        }
    }
    Ok(joined)
}

/// Compare two OS-normalized paths.
///
/// A `\\?\` verbatim form `dunce` could not shorten (over 260 characters) will
/// not compare equal to the joined path, and that refusal is deliberate: an
/// un-de-verbatim-able path is fail-closed, not "probably fine".
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    } else {
        a == b
    }
}

/// Create (or truncate) `rel` beneath `root`. The parent is reached by the same
/// no-follow descent; the leaf is created without following, so an existing
/// symlink there fails instead of writing through to its target.
///
/// INVARIANT: a write never follows a symlink, at any component, including the
/// final one. Enforced by `write_through_a_symlink_does_not_touch_the_target`
/// and `write_creates_parents_but_never_through_a_link`.
pub(crate) fn create_beneath(root: &Path, rel: &Path, mkparents: bool) -> Result<File, GuardError> {
    let (dir, leaf) = split_parent(root, rel, mkparents)?;
    // A symlink standing where the leaf should be is refused, never truncated
    // through: on Windows FILE_OVERWRITE_IF supersedes the link before the
    // reparse check runs, so guard the final component here as replace_beneath
    // does. (Unix's O_NOFOLLOW already refuses, harmlessly, one layer down.)
    match dir.child_kind(&leaf) {
        None | Some(NodeKind::RegularFile) => {}
        Some(NodeKind::NotFollowable { .. }) => {
            return Err(GuardError::Escape {
                at: rel.to_path_buf(),
            })
        }
        Some(_) => return Err(GuardError::NotRegular),
    }
    dir.create_child_file(&leaf, Excl::Truncate)
        .map_err(|e| at(e, rel))
}

/// Atomically replace `rel`'s contents: write a sibling temp file, sync it,
/// rename over the target, then make the *name* durable.
///
/// INVARIANT: a crash mid-write leaves either the old file or the new one,
/// never a truncated one; the target's attributes survive the swap; and a
/// symlink standing where the target should be is refused rather than followed
/// *or* silently replaced (the rename does not follow links, so without the
/// check an atomic replace would quietly delete the link). Enforced by
/// `replace_is_atomic_and_durable` and
/// `edit_inside_the_tree_still_works_and_keeps_the_mode`.
pub(crate) fn replace_beneath(root: &Path, rel: &Path, bytes: &[u8]) -> Result<(), GuardError> {
    use std::io::Write;
    let (dir, leaf) = split_parent(root, rel, false)?;
    // What is standing there now, if anything — checked before a byte is
    // staged, so a refusal leaves no debris.
    match dir.child_kind(&leaf) {
        None | Some(NodeKind::RegularFile) => {}
        Some(NodeKind::NotFollowable { .. }) => {
            return Err(GuardError::Escape {
                at: rel.to_path_buf(),
            })
        }
        // Catch-all refuses, per `NodeKind`'s contract: a variant added later
        // must fail closed here rather than fall through to a replace.
        Some(_) => return Err(GuardError::NotRegular),
    }
    let existing = dir.child_attrs(&leaf);
    let tmp = std::ffi::OsString::from(format!(
        ".hotl-tmp-{}-{}",
        std::process::id(),
        next_tmp_seq()
    ));
    let mut f = dir
        .create_child_file(&tmp, Excl::MustNotExist)
        .map_err(|e| at(e, rel))?;
    // Carry the target's attributes across, or an atomic replace would silently
    // drop the executable bit off every script it edits.
    let staged = (|| {
        if let Some(attrs) = existing {
            dir.apply_attrs(&f, attrs)?;
        }
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = staged {
        dir.unlink_child(&tmp);
        return Err(GuardError::Io(e));
    }
    drop(f);
    if let Err(e) = dir.rename_child(&tmp, &leaf) {
        dir.unlink_child(&tmp);
        return Err(GuardError::Io(e.error));
    }
    // Durability of the *name*: without this the rename can be lost on a
    // platform that does not journal it, even though the data was synced.
    dir.sync_name_durability().map_err(|e| at(e, rel))
}

/// Descend to `rel`'s parent and hand back `(parent handle, leaf name)`.
/// Missing intermediates are created when `mkparents`, and every one that
/// already exists is re-opened without following, so a symlinked intermediate
/// is refused rather than descended through.
fn split_parent(
    root: &Path,
    rel: &Path,
    mkparents: bool,
) -> Result<(Dir, std::ffi::OsString), GuardError> {
    let comps = components_of(rel)?;
    // A bare `.` names the root directory, which is not a file to write.
    let Some((leaf, parents)) = comps.split_last() else {
        return Err(GuardError::Escape {
            at: rel.to_path_buf(),
        });
    };
    let mut dir = Dir::open_root(root).map_err(|e| at(e, root))?;
    let mut walked = PathBuf::new();
    for comp in parents {
        walked.push(comp);
        dir = open_or_make_dir(&dir, comp, mkparents, &walked)?;
    }
    Ok((dir, (*leaf).to_os_string()))
}

fn open_or_make_dir(
    dir: &Dir,
    comp: &OsStr,
    mkparents: bool,
    walked: &Path,
) -> Result<Dir, GuardError> {
    match dir.open_child_dir(comp) {
        Ok(d) => Ok(d),
        Err(e) if mkparents && e.error.kind() == std::io::ErrorKind::NotFound => {
            // Racing creators are fine — the re-open below is what decides.
            let _ = dir.make_child_dir(comp);
            dir.open_child_dir(comp).map_err(|e| at(e, walked))
        }
        Err(e) => Err(at(e, walked)),
    }
}

/// `root.join(rel)` with the lone `.` collapsed, so a caller handing the result
/// to an external walker gets `root` rather than `root/.`.
pub(crate) fn join_beneath(root: &Path, rel: &Path) -> PathBuf {
    if rel.as_os_str() == "." {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

fn open_leaf(root: &Path, rel: &Path, mode: OpenMode) -> Result<File, GuardError> {
    // Run the lexical refusals before any syscall, so layer 2 cannot resolve a
    // name layer 3 would have rejected.
    let comps = components_of(rel)?;
    let rootdir = Dir::open_root(root).map_err(|e| at(e, root))?;
    // Layer 2 — one syscall, one resolution, kernel-enforced beneath-ness.
    if let Some(f) = rootdir.resolve_beneath(rel, mode).map_err(|e| at(e, rel))? {
        return Ok(f);
    }
    // Layer 3 — portable: walk the components, refusing a link at every step.
    let Some((last, parents)) = comps.split_last() else {
        return Ok(rootdir.into_file()); // "." — the root handle itself
    };
    let mut dir = rootdir;
    let mut walked = PathBuf::new();
    for comp in parents {
        walked.push(comp);
        dir = dir.open_child_dir(comp).map_err(|e| at(e, &walked))?;
    }
    walked.push(last);
    dir.open_child_file(last, mode).map_err(|e| at(e, &walked))
}

/// Per-process counter so two concurrent replaces cannot pick the same temp
/// name (the pid alone is not enough).
fn next_tmp_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testsupport::symlink_or_skip;

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
            dunce::canonicalize(root).unwrap(),
            dunce::canonicalize(home).unwrap(),
        )
    }

    /// The four anchored Windows forms. `classify` already refuses every one
    /// of them — `Component::RootDir | Component::Prefix(_) => Outside` catches
    /// them on Windows, including `C:foo`, which `is_absolute()` calls false
    /// but which still yields a `Prefix`. This test pins that behavior rather
    /// than changing it, and covers the Unix side too, where the `:` filter is
    /// what refuses them.
    #[test]
    fn anchored_windows_forms_never_resolve_to_something_inside() {
        let (_o, root, _home) = fixture();
        for path in [
            r"C:foo",                  // drive-relative: resolves against a cwd that is not ours
            r"C:\Windows\notepad.exe", // drive-absolute
            r"\\srv\share\x",          // UNC
            r"\\?\C:\x",               // verbatim
            r"\\.\PIPE\x",             // device namespace
        ] {
            let placed = classify(&root, path);
            if let Placement::Inside(rel) = placed {
                // On Unix these are ordinary (if odd) relative names, so the
                // guard must still refuse them at the door.
                let err = open_beneath(&root, &rel).unwrap_err();
                assert!(
                    matches!(err, GuardError::BadName { .. } | GuardError::Io(_)),
                    "`{path}` classified Inside and then opened: {err:?}"
                );
            }
        }
    }

    /// The lexical refusals, exercised through the guard rather than through
    /// the platform crate, and on **create** as well as read — `create_beneath`
    /// and `replace_beneath` had no such check when the guard was Unix-only,
    /// and a reserved device name only bites on the way in.
    #[test]
    fn names_windows_would_resolve_differently_are_refused_on_read_and_on_create() {
        let (_o, root, _home) = fixture();
        for name in ["notes.md:evil", "notes.md.", "notes.md ", "CON", "nul.txt"] {
            let rel = Path::new(name);
            assert!(
                matches!(open_beneath(&root, rel), Err(GuardError::BadName { .. })),
                "read of `{name}` must be refused"
            );
            assert!(
                matches!(
                    create_beneath(&root, rel, false),
                    Err(GuardError::BadName { .. })
                ),
                "create of `{name}` must be refused"
            );
            assert!(
                matches!(
                    replace_beneath(&root, rel, b"x"),
                    Err(GuardError::BadName { .. })
                ),
                "replace of `{name}` must be refused"
            );
            assert!(
                !root.join(name).exists(),
                "`{name}` was refused but something was created anyway"
            );
        }
        // And an ordinary name still works, so the filter is not a blanket.
        assert!(create_beneath(&root, Path::new("notes.md"), false).is_ok());
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
        symlink_or_skip!(&home, root.join("link"));
        // The tier1 lexical check passes it — this is the bug, reproduced.
        assert!(matches!(classify(&root, "link"), Placement::Inside(_)));
        // The fd check refuses it. No race, no timing, no retry.
        let err = open_dir_beneath(&root, std::path::Path::new("link")).unwrap_err();
        assert!(matches!(err, GuardError::Escape { .. }), "{err:?}");
    }

    #[test]
    fn symlinked_leaf_is_refused() {
        let (_o, root, home) = fixture();
        symlink_or_skip!(home.join("id_rsa"), root.join("notes.md"));
        let err = open_beneath(&root, std::path::Path::new("notes.md")).unwrap_err();
        assert!(matches!(err, GuardError::Escape { .. }), "{err:?}");
    }

    #[test]
    fn symlinked_intermediate_component_is_refused() {
        let (_o, root, home) = fixture();
        std::fs::write(home.join("secret.txt"), "s").unwrap();
        symlink_or_skip!(&home, root.join("src/out"));
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
        symlink_or_skip!(root.join("src/lib.rs"), root.join("alias.rs"));
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

    /// Unix-only because a FIFO is. The generalization — a non-regular file is
    /// refused rather than read — is covered on Windows by the reserved
    /// device-name refusal (`NUL`, `CON`), which is where that class actually
    /// shows up there.
    #[test]
    #[cfg(unix)]
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
        symlink_or_skip!(home.join("id_rsa"), root.join("notes.md"));
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
        let sandbox_root = dunce::canonicalize(std::env::current_dir().unwrap()).unwrap();
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

    // POSIX absolute paths like `/srv/cache` are not `is_absolute()` on Windows
    // (no drive prefix), so the guard rejects them before the prefix match.
    #[cfg(unix)]
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
        // Same object by name and by the handle the guard validated. Goes
        // through the platform seam rather than `MetadataExt`, so this is the
        // `FILE_ID_INFO` twin on Windows and the `(dev, ino)` one on Unix —
        // one assertion, two identity schemes.
        let by_name = hotl_platform::openat::identity_at(&joined).unwrap();
        let by_handle = hotl_platform::openat::identity_of(
            &open_beneath(&root, std::path::Path::new("src/lib.rs")).unwrap(),
        )
        .unwrap();
        assert_eq!(by_name, by_handle);
        // `.` resolves to the root itself, not `root/.`.
        assert_eq!(
            resolve_beneath(&root, std::path::Path::new(".")).unwrap(),
            root
        );
        // And a symlinked component never resolves at all.
        symlink_or_skip!(&home, root.join("link"));
        assert!(matches!(
            resolve_beneath(&root, std::path::Path::new("link/id_rsa")),
            Err(GuardError::Escape { .. })
        ));
    }
}
