//! Per-sub-agent git worktrees (0024): an isolated child edits its own
//! checkout instead of the parent's working tree, and its diff is applied
//! back — whole, or not at all — when it finishes. Since 0065 the main
//! session can have one too ([`SessionWorktree`]): named, on a branch, not
//! seeded, locked while a session owns it.
//!
//! Owns hotl's "shell out to the git CLI, degrade to `None` if it isn't
//! there" posture via [`git_available`].
//!
//! Two things about the location are load-bearing, not incidental:
//!
//! 1. The worktree is under `<workspace>/.git/`, so it is **inside the kernel
//!    write floor** (`hotl_tools::sandbox` builds that floor from the process
//!    cwd, set once at startup — a path discovered later can never be added).
//! 2. Under `.git/` specifically, so it is already invisible to `git status`,
//!    `glob` (which skips `.git`), and `rg`. A worktree at
//!    `<workspace>/.hotl/` would need a `.git/info/exclude` write, which reads
//!    as cosmetic and is not: delete it later and the parent's `glob`/`grep`
//!    silently return one copy of the repo per live child.
//!
//! Every git invocation here passes an explicit `-C <path>` and never relies
//! on an ambient `GIT_DIR`/`GIT_WORK_TREE` — a repo convention, earned when a
//! test escaped its tempdir through an inherited `GIT_DIR`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Whether the git CLI is on PATH. Worktree isolation (and anything else
/// that shells out to git) degrades to shared-cwd when it isn't.
pub fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Where child worktrees live, relative to the workspace root.
const WORKTREE_DIR: &str = ".git/hotl-worktrees";
/// The same, relative to the git dir.
const WORKTREE_SUBDIR: &str = "hotl-worktrees";

/// `rev-parse --show-toplevel`, canonical. `None` outside a git worktree.
fn toplevel(dir: &Path) -> Option<PathBuf> {
    let top = git_ok(dir, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(String::from_utf8(top.stdout).ok()?.trim());
    dunce::canonicalize(&top).ok().or(Some(top))
}

/// The repository's one `.git` directory, absolute — the main checkout's even
/// when `dir` is a linked worktree. `--git-common-dir` is relative unless
/// `--path-format=absolute` (git ≥ 2.31), so both spellings are handled.
fn git_common_dir(toplevel: &Path) -> Option<PathBuf> {
    let out = git_ok(
        toplevel,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .or_else(|| git_ok(toplevel, &["rev-parse", "--git-common-dir"]))?;
    let raw = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    let abs = if raw.is_absolute() {
        raw
    } else {
        toplevel.join(raw)
    };
    dunce::canonicalize(&abs).ok().or(Some(abs))
}

pub struct Worktree {
    path: PathBuf,
    workspace: PathBuf,
    /// Tree OID of the seeded state. The child's diff is taken against this,
    /// never against `HEAD` — see [`Worktree::create`] step 5.
    baseline: String,
    /// Entries the seed could not carry across, one human-readable line each.
    ///
    /// Non-empty means the child started from *less* than the parent's working
    /// tree, which is a degradation the caller has to be able to say out loud.
    /// The only current source is a Windows symlink without the privilege to
    /// create one — see `copy_preserving_symlinks`.
    skipped: Vec<String>,
}

impl Worktree {
    /// Creates the worktree **and seeds it with the parent's live working
    /// tree**.
    ///
    /// `None` when git is unavailable, `workspace` is not inside a git
    /// worktree, `worktree add` fails, or seeding fails — in every case the
    /// caller runs shared-cwd instead.
    ///
    /// INVARIANT: a worktree that could not be seeded is removed before
    /// returning `None`. A stale-base child is worse than no isolation at all,
    /// because its edits look authoritative and are computed against a tree
    /// the parent abandoned. Enforced by
    /// `create_removes_the_worktree_when_seeding_fails`.
    pub fn create(workspace: &Path, id: &str) -> Option<Self> {
        if !git_available() {
            return None;
        }
        // Not inside a git worktree ⇒ nothing to isolate against.
        let top = toplevel(workspace)?;
        // From below the toplevel the child would land outside the kernel
        // floor (the process cwd): degrade to shared-cwd (0065 T2).
        if dunce::canonicalize(workspace).ok()? != top {
            return None;
        }
        let workspace = top;
        // Under the **common** git dir, not `<toplevel>/.git`: inside a linked
        // worktree (a 0065 session) `.git` is a file, and the common dir is
        // the main repo's — still under the floor (0065 T2).
        let path = git_common_dir(&workspace)?.join(WORKTREE_SUBDIR).join(id);
        // `--detach`: no branch is created, so the user's ref namespace stays
        // clean. The id is a fresh ULID per child — `worktree add` refuses an
        // existing path and reuse is never attempted.
        git_ok(
            &workspace,
            &[
                "worktree",
                "add",
                "--detach",
                &path.display().to_string(),
                "HEAD",
            ],
        )?;

        let wt = Self {
            path,
            workspace,
            baseline: String::new(),
            skipped: Vec::new(),
        };
        match wt.seed() {
            Some((baseline, skipped)) => Some(Self {
                baseline,
                skipped,
                ..wt
            }),
            None => {
                wt.remove();
                None
            }
        }
    }

    /// Steps 3–5 of `create`: carry the parent's uncommitted state across, then
    /// record what the child started from.
    fn seed(&self) -> Option<(String, Vec<String>)> {
        // 3. Tracked changes — modifications, deletions, and mode changes, all
        //    in one patch. `--binary` is not optional: without it a dirty
        //    binary file fails to seed and the child silently sees the `HEAD`
        //    version. The patch moves over **stdin**; `git -C <dir> apply
        //    p.patch` would resolve `p.patch` inside `<dir>`.
        let tracked = git_ok(&self.workspace, &["diff", "HEAD", "--binary"])?;
        if !tracked.stdout.is_empty() {
            apply_patch(&self.path, &tracked.stdout).ok()?;
        }

        // 4. Untracked-but-not-ignored files. Ignored files are deliberately
        //    left behind — that is what makes `target/` and `node_modules/`
        //    free. The cost is real and documented: a child cannot read
        //    `.env` either.
        //    `-z` because a filename may contain a newline.
        let others = git_ok(
            &self.workspace,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        let mut skipped = Vec::new();
        for name in others.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let rel = Path::new(std::str::from_utf8(name).ok()?);
            match copy_preserving_symlinks(&self.workspace.join(rel), &self.path.join(rel))? {
                Seeded::Copied => {}
                Seeded::Skipped(why) => skipped.push(why),
            }
        }
        // The worktree lives under `.git/`, which `ls-files --others` never
        // lists — so no self-copy guard is needed. Second dividend of the
        // location choice.

        // 5. Record the baseline. A **tree** object, not a commit: no
        //    `user.email` requirement, no `pre-commit`/`commit-msg` hook fired
        //    in the user's repo, no ref moved. The objects become unreachable
        //    when the worktree is removed and are collected by the next `git gc`.
        //
        //    Without this the child's diff would replay the seeded parent
        //    changes back into the parent: `diff --cached` against `HEAD`
        //    cannot tell the child's edits from the seed.
        git_ok(&self.path, &["add", "-A"])?;
        let tree = git_ok(&self.path, &["write-tree"])?;
        Some((
            String::from_utf8(tree.stdout).ok()?.trim().to_string(),
            skipped,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What the seed could not carry across. Empty on a clean seed.
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// The child's own work as a patch. Staged first, so files the child
    /// created appear; taken against `baseline`, so the seeded parent state
    /// does not. `None` = the child changed nothing.
    pub fn diff(&self) -> Option<String> {
        git_ok(&self.path, &["add", "-A"])?;
        let out = git_ok(
            &self.path,
            &["diff-index", "-p", "--binary", "--cached", &self.baseline],
        )?;
        let patch = String::from_utf8(out.stdout).ok()?;
        (!patch.trim().is_empty()).then_some(patch)
    }

    /// Applies the child's diff to the parent's working tree.
    ///
    /// INVARIANT: all-or-nothing, and the index is never touched. `--check`
    /// first for a clean diagnosis, then plain `git apply` — never `--3way`
    /// (writes conflict markers on failure, implies `--index` so every applied
    /// file is left staged on success, and refuses outright on a dirty parent,
    /// which is hotl's normal case) and never `--reject`. `Ok(n)` = n files
    /// changed; `Err(msg)` = nothing applied, parent working tree **and** index
    /// byte-for-byte untouched. Enforced by
    /// `apply_to_workspace_on_conflict_leaves_the_parent_untouched` and
    /// `apply_to_workspace_stages_nothing_on_success`.
    pub fn apply_to_workspace(&self) -> Result<usize, String> {
        let Some(patch) = self.diff() else {
            return Ok(0);
        };
        let bytes = patch.as_bytes();
        let files = patch
            .lines()
            .filter(|l| l.starts_with("diff --git "))
            .count();

        let check = apply_patch_args(&self.workspace, bytes, &["--check"])
            .map_err(|e| format!("could not run git apply: {e}"))?;
        if !check.status.success() {
            return Err(first_line(&check.stderr));
        }
        let out = apply_patch_args(&self.workspace, bytes, &[])
            .map_err(|e| format!("could not run git apply: {e}"))?;
        if !out.status.success() {
            return Err(first_line(&out.stderr));
        }
        Ok(files)
    }

    /// `git worktree remove --force` then `prune`. Infallible from the
    /// caller's view: a leaked worktree is a wart, not a failure worth
    /// propagating into a sub-agent's result.
    pub fn remove(self) {
        let _ = git_ok(
            &self.workspace,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.display().to_string(),
            ],
        );
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = git_ok(&self.workspace, &["worktree", "prune"]);
    }
}

/// Where a session worktree lives (0065): the same directory as the
/// children's, named after its branch.
pub fn session_path(toplevel: &Path, name: &str) -> PathBuf {
    toplevel.join(WORKTREE_DIR).join(name)
}

/// The branch `-w` with no name uses: never collides with a human's.
pub fn derived_branch(session_id: &str) -> String {
    format!("hotl/{session_id}")
}

/// The main session's git worktree (0065): on a branch, named, not seeded,
/// locked with the session's id and pid while the session owns it. A
/// [`Worktree`] (a child's) is none of those, so the two stay two types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWorktree {
    path: PathBuf,
    workspace: PathBuf,
    branch: String,
}

/// How [`SessionWorktree::reopen`] found the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reopened {
    Existing,
    /// The directory was gone; `worktree add <path> <branch>` brought it back.
    RecreatedFromBranch,
}

/// One row of `git worktree list --porcelain` under `.git/hotl-worktrees/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// `Some` only for a lock hotl wrote; a foreign reason reads as `None`.
    pub lock: Option<Lock>,
}

/// The ownership record a session leaves in `git worktree lock --reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub session_id: String,
    pub pid: u32,
}

impl SessionWorktree {
    /// `git -C <workspace> worktree add -b <branch> <path> HEAD` — the local
    /// `HEAD`, never a remote ref. A branch that already exists and is free is
    /// reused (`worktree add <path> <branch>`), which is what resume relies on.
    /// `Err` is one plain sentence: git missing, `workspace` not the repo
    /// toplevel, a bad branch name, the path taken, or the branch checked out
    /// elsewhere.
    pub fn open(workspace: &Path, branch: &str) -> Result<Self, String> {
        if !git_available() {
            return Err("git is not on PATH, so a worktree cannot be made".into());
        }
        let top = toplevel(workspace)
            .ok_or_else(|| format!("{} is not inside a git repository", workspace.display()))?;
        let here = dunce::canonicalize(workspace).map_err(|e| e.to_string())?;
        if here != top {
            return Err(format!(
                "run hotl -w from the repository root: {}",
                top.display()
            ));
        }
        if git_ok(&top, &["check-ref-format", "--branch", branch]).is_none() {
            return Err(format!("`{branch}` is not a valid git branch name"));
        }
        let path = session_path(&top, branch);
        if path.exists() {
            return Err(format!(
                "{} already exists; `hotl -r` reopens a session's worktree, `hotl gc` removes a stale one",
                path.display()
            ));
        }
        let path_s = path.display().to_string();
        // A fresh branch first; an existing one is reused if nothing has it
        // checked out. git's own stderr becomes the sentence either way.
        let fresh = git(&top, &["worktree", "add", "-b", branch, &path_s, "HEAD"])
            .ok_or("could not run git")?;
        if !fresh.status.success() {
            let reuse =
                git(&top, &["worktree", "add", &path_s, branch]).ok_or("could not run git")?;
            if !reuse.status.success() {
                let why = first_line(&reuse.stderr);
                let why = if why == "git apply failed" {
                    first_line(&fresh.stderr)
                } else {
                    why
                };
                return Err(format!("could not open a worktree on `{branch}`: {why}"));
            }
        }
        Ok(Self {
            path,
            workspace: top,
            branch: branch.to_string(),
        })
    }

    /// Reopen a known pair. A directory that is gone is recreated from its
    /// branch (the work is on the branch; the directory is disposable); a
    /// branch that is gone is an `Err`.
    pub fn reopen(workspace: &Path, path: &Path, branch: &str) -> Result<(Self, Reopened), String> {
        if !git_available() {
            return Err("git is not on PATH, so a worktree cannot be reopened".into());
        }
        let top = toplevel(workspace)
            .ok_or_else(|| format!("{} is not inside a git repository", workspace.display()))?;
        let registered = list_session_worktrees(&top)
            .into_iter()
            .any(|l| l.path == path && path.is_dir());
        if registered {
            return Ok((
                Self {
                    path: path.to_path_buf(),
                    workspace: top,
                    branch: branch.to_string(),
                },
                Reopened::Existing,
            ));
        }
        if git_ok(
            &top,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .is_none()
        {
            return Err(format!("branch `{branch}` no longer exists"));
        }
        // The lock reason outlives the directory; `prune` clears the stale
        // registration or `worktree add` reports the path as taken.
        let _ = git_ok(&top, &["worktree", "prune"]);
        let out = git(
            &top,
            &["worktree", "add", &path.display().to_string(), branch],
        )
        .ok_or("could not run git")?;
        if !out.status.success() {
            return Err(format!(
                "could not recreate the worktree on `{branch}`: {}",
                first_line(&out.stderr)
            ));
        }
        Ok((
            Self {
                path: path.to_path_buf(),
                workspace: top,
                branch: branch.to_string(),
            },
            Reopened::RecreatedFromBranch,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn branch(&self) -> &str {
        &self.branch
    }
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// `git worktree lock --reason "hotl <id> pid <pid>"`: the ownership
    /// record `hotl gc` reads. Infallible to the caller.
    pub fn lock(&self, session_id: &str) {
        let _ = git_ok(
            &self.workspace,
            &["worktree", "unlock", &self.path.display().to_string()],
        );
        let _ = git_ok(
            &self.workspace,
            &[
                "worktree",
                "lock",
                "--reason",
                &lock_reason(session_id, std::process::id()),
                &self.path.display().to_string(),
            ],
        );
    }

    pub fn unlock(&self) {
        let _ = git_ok(
            &self.workspace,
            &["worktree", "unlock", &self.path.display().to_string()],
        );
    }
}

fn lock_reason(session_id: &str, pid: u32) -> String {
    format!("hotl {session_id} pid {pid}")
}

/// `hotl <id> pid <n>` → the lock; anything else is somebody else's lock.
fn parse_lock_reason(reason: &str) -> Option<Lock> {
    let mut words = reason.split_whitespace();
    (words.next()? == "hotl").then_some(())?;
    let session_id = words.next()?.to_string();
    (words.next()? == "pid").then_some(())?;
    let pid = words.next()?.parse().ok()?;
    words.next().is_none().then_some(Lock { session_id, pid })
}

/// The session worktrees of the repository `workspace` is in: every entry of
/// `git worktree list --porcelain` under `.git/hotl-worktrees/`.
pub fn list_session_worktrees(workspace: &Path) -> Vec<Listed> {
    let Some(top) = toplevel(workspace) else {
        return Vec::new();
    };
    let Some(common) = git_common_dir(&top) else {
        return Vec::new();
    };
    let under = common.join(WORKTREE_SUBDIR);
    let Some(out) = git_ok(&top, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    parse_worktree_list(&String::from_utf8_lossy(&out.stdout), &under)
}

/// The porcelain format: blocks separated by a blank line; `worktree <path>`,
/// `HEAD <sha>`, `branch refs/heads/<b>` or `detached`, `locked` alone or
/// `locked <reason>`.
fn parse_worktree_list(text: &str, under: &Path) -> Vec<Listed> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut path: Option<PathBuf> = None;
        let mut branch = None;
        let mut lock = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if let Some(rest) = line.strip_prefix("locked") {
                lock = parse_lock_reason(rest.trim());
            }
        }
        let Some(path) = path else { continue };
        let canon = dunce::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if canon.starts_with(under) || path.starts_with(under) {
            out.push(Listed {
                path: canon,
                branch,
                lock,
            });
        }
    }
    out
}

fn git(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Never an ambient repo: a test once escaped its tempdir this way.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()
}

/// `git`, but a non-zero exit is a failure too. `git` alone only reports
/// whether the process spawned — the distinction that made T2-15b silent.
fn git_ok(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    git(dir, args).filter(|o| o.status.success())
}

fn apply_patch(dir: &Path, patch: &[u8]) -> Result<(), String> {
    let out = apply_patch_args(dir, patch, &[]).map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(first_line(&out.stderr))
    }
}

/// The one place a patch reaches git. Always over **stdin**: `git -C <dir>
/// apply p.patch` resolves `p.patch` relative to `<dir>`, which reads the
/// wrong file (or none) — loudly in a test, silently in a repo that happens
/// to contain a matching name.
fn apply_patch_args(
    dir: &Path,
    patch: &[u8],
    extra: &[&str],
) -> std::io::Result<std::process::Output> {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("apply")
        .arg("--binary")
        .args(extra)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(patch)?;
    child.wait_with_output()
}

/// What became of one seeded entry, when the outcome is survivable.
enum Seeded {
    Copied,
    /// The entry was left out of the child and the caller must say so.
    ///
    /// There is deliberately **no** variant that falls back to copying a
    /// symlink's contents: materializing the target's bytes inside the child is
    /// exactly the out-of-repo smuggling `copy_preserving_symlinks` exists to
    /// prevent, and doing it quietly would turn a visible degradation into an
    /// invisible one.
    Skipped(String),
}

/// Copy `from` to `to`, creating parent directories. A symlink is recreated as
/// a symlink: a naive copy would dereference it and materialize the target's
/// bytes inside the child, which is both wrong and a way to smuggle a file
/// from outside the repo into the diff.
fn copy_preserving_symlinks(from: &Path, to: &Path) -> Option<Seeded> {
    let meta = std::fs::symlink_metadata(from).ok()?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(from).ok()?;
        let _ = std::fs::remove_file(to);
        if let Err(e) = create_symlink(&target, to, from) {
            if is_privilege_not_held(&e) {
                return Some(Seeded::Skipped(format!(
                    "{}: not seeded — creating a symlink on Windows needs Developer Mode or \
                     SeCreateSymbolicLinkPrivilege",
                    from.display()
                )));
            }
            return None;
        }
    } else if meta.is_file() {
        std::fs::copy(from, to).ok()?;
    }
    Some(Seeded::Copied)
}

#[cfg(unix)]
fn create_symlink(target: &Path, to: &Path, _from: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, to)
}

/// Windows wants the link kind up front, and gets it wrong in a way that
/// matters: a file symlink pointing at a directory does not resolve. Decide
/// from what the *source* link resolves to, which is `target` interpreted
/// relative to the original link's parent.
#[cfg(windows)]
fn create_symlink(target: &Path, to: &Path, from: &Path) -> std::io::Result<()> {
    let resolved = from.parent().map(|p| p.join(target));
    let is_dir = resolved
        .and_then(|p| std::fs::metadata(p).ok())
        .is_some_and(|m| m.is_dir());
    if is_dir {
        std::os::windows::fs::symlink_dir(target, to)
    } else {
        std::os::windows::fs::symlink_file(target, to)
    }
}

/// `ERROR_PRIVILEGE_NOT_HELD`. On a stock Windows install this is the *default*
/// outcome rather than an exotic one — symlink creation is an administrator
/// privilege unless Developer Mode is on — which is why it degrades to a
/// reported skip instead of failing the whole worktree build.
fn is_privilege_not_held(e: &std::io::Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(1314)
}

fn first_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("git apply failed")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch repo with one commit. Returns `None` when git is missing, so
    /// every test here skips rather than fails on a host without it.
    fn repo() -> Option<tempfile::TempDir> {
        if !git_available() {
            return None;
        }
        let tmp = tempfile::tempdir().ok()?;
        let dir = tmp.path();
        git_ok(dir, &["init", "-q", "-b", "main"])?;
        git_ok(dir, &["config", "user.email", "t@example.com"])?;
        git_ok(dir, &["config", "user.name", "t"])?;
        // Git for Windows defaults core.autocrlf on, which rewrites seeded and
        // merged-back files to CRLF and breaks the byte-exact compares below.
        git_ok(dir, &["config", "core.autocrlf", "false"])?;
        std::fs::write(dir.join("a.txt"), "a1\na2\na3\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b1\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        git_ok(dir, &["add", "-A"])?;
        git_ok(dir, &["commit", "-qm", "init"])?;
        Some(tmp)
    }

    macro_rules! repo_or_skip {
        () => {
            match repo() {
                Some(r) => r,
                None => return,
            }
        };
    }

    fn worktree_count(dir: &Path) -> usize {
        let out = git_ok(dir, &["worktree", "list"]).unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }

    fn staged(dir: &Path) -> String {
        let out = git_ok(dir, &["diff", "--cached", "--name-only"]).unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn create_then_remove_leaves_no_registration() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        assert_eq!(worktree_count(root), 1);
        let wt = Worktree::create(root, "01JTESTA").unwrap();
        assert!(wt.path().is_dir());
        assert_eq!(worktree_count(root), 2);
        let path = wt.path().to_path_buf();
        wt.remove();
        assert_eq!(worktree_count(root), 1);
        assert!(!path.exists());
    }

    /// The regression test for the constraint the whole design turns on: a
    /// hotl session's tree is dirty by construction, so a child checked out at
    /// `HEAD` reads pre-edit files and diffs against a base the parent
    /// abandoned turns ago. Without this the feature is wrong in the only
    /// environment it ships into, and every other check here still passes.
    #[test]
    fn create_seeds_the_parents_uncommitted_modifications() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "a1\nPARENT EDIT\na3\n").unwrap();

        let wt = Worktree::create(root, "01JTESTB").unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.path().join("a.txt")).unwrap(),
            "a1\nPARENT EDIT\na3\n",
            "the child was handed the HEAD version, not the parent's live tree"
        );
        wt.remove();
    }

    #[test]
    fn create_seeds_deletions_binaries_and_untracked_files() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        // A binary file, committed clean and then made dirty — the case that
        // needs `--binary` on the seeding diff.
        std::fs::write(root.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        git_ok(root, &["add", "-A"]).unwrap();
        git_ok(root, &["commit", "-qm", "blob"]).unwrap();
        std::fs::write(root.join("blob.bin"), [0u8, 1, 2, 3, 255]).unwrap();

        std::fs::remove_file(root.join("b.txt")).unwrap();
        std::fs::write(root.join("new.txt"), "fresh\n").unwrap();

        let wt = Worktree::create(root, "01JTESTC").unwrap();
        assert!(!wt.path().join("b.txt").exists(), "deletion did not seed");
        assert_eq!(
            std::fs::read(wt.path().join("blob.bin")).unwrap(),
            vec![0u8, 1, 2, 3, 255],
            "dirty binary did not seed"
        );
        assert_eq!(
            std::fs::read_to_string(wt.path().join("new.txt")).unwrap(),
            "fresh\n",
            "untracked file did not seed"
        );
        wt.remove();
    }

    #[test]
    fn create_does_not_seed_gitignored_files() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        std::fs::write(root.join("ignored.txt"), "secret\n").unwrap();

        let wt = Worktree::create(root, "01JTESTD").unwrap();
        assert!(
            !wt.path().join("ignored.txt").exists(),
            "an ignored file was copied into the child"
        );
        wt.remove();
    }

    /// Pins the baseline tree. Without it `diff` replays the seeded parent
    /// state back into the parent, which `git apply` then rejects as already
    /// applied — an isolation feature that only ever reports conflicts.
    #[test]
    fn diff_excludes_the_seeded_parent_state() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "a1\nPARENT\na3\n").unwrap();

        let wt = Worktree::create(root, "01JTESTE").unwrap();
        std::fs::write(wt.path().join("b.txt"), "b1\nCHILD\n").unwrap();

        let patch = wt.diff().expect("the child changed b.txt");
        assert!(patch.contains("b.txt"), "{patch}");
        assert!(
            !patch.contains("a.txt"),
            "the seed leaked into the diff:\n{patch}"
        );
        wt.remove();
    }

    #[test]
    fn diff_includes_new_and_modified_files() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let wt = Worktree::create(root, "01JTESTF").unwrap();
        assert!(wt.diff().is_none(), "an untouched child has no diff");

        std::fs::write(wt.path().join("a.txt"), "a1\nCHILD\na3\n").unwrap();
        std::fs::write(wt.path().join("made.txt"), "new\n").unwrap();
        let patch = wt.diff().unwrap();
        assert!(
            patch.contains("a.txt") && patch.contains("made.txt"),
            "{patch}"
        );
        wt.remove();
    }

    /// The case `--3way` refuses outright (`does not match index`), and the
    /// normal one for hotl: the parent has uncommitted work of its own while a
    /// child runs.
    #[test]
    fn apply_to_workspace_lands_the_childs_edit_in_a_dirty_parent() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let wt = Worktree::create(root, "01JTESTG").unwrap();
        std::fs::write(wt.path().join("b.txt"), "b1\nCHILD\n").unwrap();
        // The parent edits an unrelated file after the child was seeded.
        std::fs::write(root.join("a.txt"), "a1\nPARENT LATER\na3\n").unwrap();

        assert_eq!(wt.apply_to_workspace(), Ok(1));
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "b1\nCHILD\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "a1\nPARENT LATER\na3\n",
            "the parent's own edit was clobbered"
        );
        wt.remove();
    }

    #[test]
    fn apply_to_workspace_on_conflict_leaves_the_parent_untouched() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let wt = Worktree::create(root, "01JTESTH").unwrap();
        std::fs::write(wt.path().join("a.txt"), "a1\nCHILD\na3\n").unwrap();
        // The parent moves the same line out from under the child.
        std::fs::write(root.join("a.txt"), "a1\nPARENT\na3\n").unwrap();
        let before = std::fs::read_to_string(root.join("a.txt")).unwrap();

        let err = wt.apply_to_workspace().unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            before,
            "a refused apply wrote to the parent anyway"
        );
        assert_eq!(staged(root), "", "a refused apply staged something");
        // The child's work survives, in its own worktree, for the human.
        assert!(wt.path().join("a.txt").exists());
        wt.remove();
    }

    /// Pins the no-`--index` property, which a working-tree-only assertion
    /// cannot see: a staging area silently mutated by a sub-agent is what the
    /// parent's own `git checkout` would not reverse.
    #[test]
    fn apply_to_workspace_stages_nothing_on_success() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let wt = Worktree::create(root, "01JTESTI").unwrap();
        std::fs::write(wt.path().join("b.txt"), "b1\nCHILD\n").unwrap();

        assert_eq!(wt.apply_to_workspace(), Ok(1));
        assert_eq!(staged(root), "");
        wt.remove();
    }

    #[test]
    fn create_returns_none_outside_a_git_repo() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        assert!(Worktree::create(tmp.path(), "01JTESTJ").is_none());
    }

    /// A half-seeded worktree must never be handed to a child: its files look
    /// authoritative and are a mix of two trees. The failure is forced with an
    /// unreadable untracked file, which is the one seeding step that touches
    /// the filesystem directly rather than through git.
    #[test]
    fn create_removes_the_worktree_when_seeding_fails() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let locked = root.join("locked.txt");
        std::fs::write(&locked, "no\n").unwrap();
        let Some(guard) = make_uncopyable(&locked) else {
            return; // running as root, or the platform will not refuse the read
        };

        assert!(Worktree::create(root, "01JTESTK").is_none());
        assert_eq!(worktree_count(root), 1, "a half-seeded worktree leaked");
        assert!(!root.join(WORKTREE_DIR).join("01JTESTK").exists());

        drop(guard);
    }

    /// Make `path` unreadable to the seeding copy, or return `None` if this
    /// environment will not cooperate. Dropping the returned value undoes it.
    ///
    /// Two mechanisms because the platforms refuse for different reasons:
    /// `0o000` denies by permission, which root ignores, while a Windows share
    /// mode of 0 denies by *sharing*, which nothing overrides — so the Windows
    /// twin is actually the more reliable of the two.
    #[cfg(unix)]
    fn make_uncopyable(path: &Path) -> Option<impl Sized> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).ok()?;
        if std::fs::read(path).is_ok() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
            return None;
        }
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644));
            }
        }
        Some(Restore(path.to_path_buf()))
    }

    #[cfg(windows)]
    fn make_uncopyable(path: &Path) -> Option<impl Sized> {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .ok()
    }
}

/// 0065 T1/T2 — the session worktree and the common-dir child path.
#[cfg(test)]
mod session_tests {
    use super::*;

    fn repo() -> Option<tempfile::TempDir> {
        if !git_available() {
            return None;
        }
        let tmp = tempfile::tempdir().ok()?;
        let dir = tmp.path();
        git_ok(dir, &["init", "-q", "-b", "main"])?;
        git_ok(dir, &["config", "user.email", "t@example.com"])?;
        git_ok(dir, &["config", "user.name", "t"])?;
        git_ok(dir, &["config", "core.autocrlf", "false"])?;
        std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
        git_ok(dir, &["add", "-A"])?;
        git_ok(dir, &["commit", "-qm", "init"])?;
        Some(tmp)
    }

    macro_rules! repo_or_skip {
        () => {
            match repo() {
                Some(r) => r,
                None => return,
            }
        };
    }

    fn commit(dir: &Path, file: &str, text: &str, msg: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        git_ok(dir, &["add", "-A"]).unwrap();
        git_ok(dir, &["commit", "-qm", msg]).unwrap();
    }

    fn head(dir: &Path) -> String {
        let out = git_ok(dir, &["rev-parse", "HEAD"]).unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn open_branches_from_local_head_not_origin() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        git_ok(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]).unwrap();
        commit(root, "two.txt", "two\n", "two");
        let wt = SessionWorktree::open(root, "fix-auth").unwrap();
        assert!(
            wt.path().join("two.txt").exists(),
            "branched from the local tip"
        );
        assert_eq!(head(wt.path()), head(root));
        assert_eq!(wt.branch(), "fix-auth");
        assert_eq!(wt.path(), session_path(wt.workspace(), "fix-auth"));
    }

    #[test]
    fn open_reuses_an_existing_free_branch_and_refuses_one_checked_out_elsewhere() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        git_ok(root, &["branch", "free"]).unwrap();
        let wt = SessionWorktree::open(root, "free").expect("a free branch is reused");
        assert_eq!(wt.branch(), "free");
        // The same branch twice: the second is checked out elsewhere.
        std::fs::remove_dir_all(wt.path()).unwrap();
        git_ok(root, &["worktree", "prune"]).unwrap();
        git_ok(
            root,
            &[
                "worktree",
                "add",
                &root.join("elsewhere").display().to_string(),
                "free",
            ],
        )
        .unwrap();
        let err = SessionWorktree::open(root, "free").unwrap_err();
        assert!(err.contains("free"), "{err}");
    }

    #[test]
    fn open_refuses_a_bad_branch_name_and_below_the_toplevel() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        for bad in ["a..b", "-x", "a b"] {
            let err = SessionWorktree::open(root, bad).unwrap_err();
            assert!(err.contains("branch name"), "{bad}: {err}");
        }
        let sub = root.join("crates");
        std::fs::create_dir_all(&sub).unwrap();
        let err = SessionWorktree::open(&sub, "ok").unwrap_err();
        assert!(err.contains("repository root"), "{err}");
        assert!(root
            .join(".git/hotl-worktrees")
            .read_dir()
            .map_or(true, |mut d| d.next().is_none()));
    }

    /// Phase 1's contract: uncommitted changes stay in the main checkout.
    #[test]
    fn open_does_not_seed_uncommitted_changes() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "dirty\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let wt = SessionWorktree::open(root, "clean").unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.path().join("a.txt")).unwrap(),
            "a1\n"
        );
        assert!(!wt.path().join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "dirty\n"
        );
    }

    #[test]
    fn lock_reason_names_the_session_and_pid_and_list_ignores_foreign_locks() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let wt = SessionWorktree::open(root, "fix-auth").unwrap();
        wt.lock("01JSESSION");
        let listed = list_session_worktrees(root);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].branch.as_deref(), Some("fix-auth"));
        assert_eq!(
            listed[0].lock,
            Some(Lock {
                session_id: "01JSESSION".into(),
                pid: std::process::id()
            })
        );
        // Relocking replaces the reason; unlocking clears it.
        wt.lock("01JOTHER");
        assert_eq!(
            list_session_worktrees(root)[0]
                .lock
                .as_ref()
                .unwrap()
                .session_id,
            "01JOTHER"
        );
        wt.unlock();
        assert_eq!(list_session_worktrees(root)[0].lock, None);
        // A foreign lock (a human's reason, or none) is not ours.
        git_ok(
            root,
            &[
                "worktree",
                "lock",
                "--reason",
                "mine",
                &wt.path().display().to_string(),
            ],
        )
        .unwrap();
        assert_eq!(list_session_worktrees(root)[0].lock, None);
        wt.unlock();
        git_ok(
            root,
            &["worktree", "lock", &wt.path().display().to_string()],
        )
        .unwrap();
        assert_eq!(list_session_worktrees(root)[0].lock, None);
        // A worktree outside `.git/hotl-worktrees/` is not listed.
        git_ok(
            root,
            &[
                "worktree",
                "add",
                &root.join("../other-wt").display().to_string(),
                "-b",
                "x",
            ],
        )
        .unwrap();
        assert_eq!(list_session_worktrees(root).len(), 1);
        git_ok(
            root,
            &[
                "worktree",
                "remove",
                "--force",
                &root.join("../other-wt").display().to_string(),
            ],
        )
        .unwrap();
    }

    #[test]
    fn parse_worktree_list_handles_bare_and_reasoned_locks() {
        let text = "worktree /r\nHEAD abc\nbranch refs/heads/main\n\nworktree /r/.git/hotl-worktrees/a\nHEAD abc\nbranch refs/heads/a\nlocked hotl 01J pid 42\n\nworktree /r/.git/hotl-worktrees/b\nHEAD abc\ndetached\nlocked\n\nworktree /r/.git/hotl-worktrees/c\nHEAD abc\nbranch refs/heads/c\nlocked hotl 01J pid notanumber\n";
        let rows = parse_worktree_list(text, Path::new("/r/.git/hotl-worktrees"));
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].lock,
            Some(Lock {
                session_id: "01J".into(),
                pid: 42
            })
        );
        assert_eq!(
            rows[1],
            Listed {
                path: "/r/.git/hotl-worktrees/b".into(),
                branch: None,
                lock: None
            }
        );
        assert_eq!(rows[2].lock, None);
    }

    #[test]
    fn reopen_recreates_a_missing_directory_from_its_branch_and_fails_when_it_is_gone() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let wt = SessionWorktree::open(root, "fix-auth").unwrap();
        commit(wt.path(), "wt.txt", "in the worktree\n", "wt");
        let (same, how) = SessionWorktree::reopen(root, wt.path(), "fix-auth").unwrap();
        assert_eq!((how, &same), (Reopened::Existing, &wt));
        std::fs::remove_dir_all(wt.path()).unwrap();
        let (again, how) = SessionWorktree::reopen(root, wt.path(), "fix-auth").unwrap();
        assert_eq!(how, Reopened::RecreatedFromBranch);
        assert_eq!(again.path(), wt.path());
        assert!(
            wt.path().join("wt.txt").exists(),
            "the branch's commit is checked out"
        );
        std::fs::remove_dir_all(wt.path()).unwrap();
        git_ok(root, &["worktree", "prune"]).unwrap();
        git_ok(root, &["branch", "-D", "fix-auth"]).unwrap();
        let err = SessionWorktree::reopen(root, wt.path(), "fix-auth").unwrap_err();
        assert!(err.contains("no longer exists"), "{err}");
    }

    #[test]
    fn derived_branch_is_the_session_id_under_hotl() {
        assert_eq!(derived_branch("01JABC"), "hotl/01JABC");
        assert_eq!(lock_reason("01J", 7), "hotl 01J pid 7");
    }

    /// T2: a child of a session worktree nests under the main repo's `.git`
    /// (the floor), and its diff lands in the session worktree.
    #[test]
    fn create_inside_a_linked_worktree_nests_under_the_common_git_dir() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let session = SessionWorktree::open(root, "fix-auth").unwrap();
        let child = Worktree::create(session.path(), "01JCHILD").expect("a child inside a session");
        let main_dir = dunce::canonicalize(root.join(".git/hotl-worktrees")).unwrap();
        assert!(
            child.path().starts_with(&main_dir),
            "{} is not under {}",
            child.path().display(),
            main_dir.display()
        );
        std::fs::write(child.path().join("c.txt"), "child\n").unwrap();
        assert_eq!(child.apply_to_workspace().unwrap(), 1);
        assert!(
            session.path().join("c.txt").exists(),
            "merged into the session worktree"
        );
        assert!(!root.join("c.txt").exists(), "never into the main checkout");
        child.remove();
    }

    #[test]
    fn create_degrades_when_the_cwd_is_below_the_toplevel() {
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let sub = root.join("crates/hotl");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(Worktree::create(&sub, "01JSUB").is_none());
        assert!(Worktree::create(root, "01JTOP").is_some());
    }
}
