//! Per-sub-agent git worktrees (0024): an isolated child edits its own
//! checkout instead of the parent's working tree, and its diff is applied
//! back — whole, or not at all — when it finishes.
//!
//! Lives next to [`shadow`](crate::shadow) because that module already owns
//! hotl's "shell out to the git CLI, degrade to `None` if it isn't there"
//! posture, and exports [`shadow::git_available`], which this reuses.
//!
//! Two things about the location are load-bearing, not incidental:
//!
//! 1. The worktree is under `<workspace>/.git/`, so it is **inside the kernel
//!    write floor** (`hotl_tools::sandbox` builds that floor from the process
//!    cwd, set once at startup — a path discovered later can never be added).
//! 2. Under `.git/` specifically, so it is already invisible to `git status`,
//!    `glob` (which skips `.git`), `rg`, and the undo snapshot's `EXCLUDES`
//!    (which already begins with `.git/`). A worktree at `<workspace>/.hotl/`
//!    would need a `.git/info/exclude` write *and* a new `EXCLUDES` entry,
//!    both of which read as cosmetic and are not: delete either later and the
//!    parent's `glob`/`grep` silently return one copy of the repo per live
//!    child.
//!
//! Every git invocation here passes an explicit `-C <path>` and never relies
//! on an ambient `GIT_DIR`/`GIT_WORK_TREE` — a repo convention, earned when a
//! test escaped its tempdir through an inherited `GIT_DIR`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where child worktrees live, relative to the workspace root.
const WORKTREE_DIR: &str = ".git/hotl-worktrees";

pub struct Worktree {
    path: PathBuf,
    workspace: PathBuf,
    /// Tree OID of the seeded state. The child's diff is taken against this,
    /// never against `HEAD` — see [`Worktree::create`] step 5.
    baseline: String,
}

impl Worktree {
    /// Creates the worktree **and seeds it with the parent's live working
    /// tree**.
    ///
    /// `None` when git is unavailable, `workspace` is not inside a git
    /// worktree, `worktree add` fails, or seeding fails — in every case the
    /// caller runs shared-cwd instead, exactly as `Shadow::create` returning
    /// `None` means the session runs without undo.
    ///
    /// INVARIANT: a worktree that could not be seeded is removed before
    /// returning `None`. A stale-base child is worse than no isolation at all,
    /// because its edits look authoritative and are computed against a tree
    /// the parent abandoned. Enforced by
    /// `create_removes_the_worktree_when_seeding_fails`.
    pub fn create(workspace: &Path, id: &str) -> Option<Self> {
        if !crate::shadow::git_available() {
            return None;
        }
        // Not inside a git worktree ⇒ nothing to isolate against.
        let top = git_ok(workspace, &["rev-parse", "--show-toplevel"])?;
        let workspace = PathBuf::from(String::from_utf8(top.stdout).ok()?.trim());

        let path = workspace.join(WORKTREE_DIR).join(id);
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
        };
        match wt.seed() {
            Some(baseline) => Some(Self { baseline, ..wt }),
            None => {
                wt.remove();
                None
            }
        }
    }

    /// Steps 3–5 of `create`: carry the parent's uncommitted state across, then
    /// record what the child started from.
    fn seed(&self) -> Option<String> {
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
        //    free, and it is the same line the undo snapshot draws. The cost is
        //    real and documented: a child cannot read `.env` either.
        //    `-z` because a filename may contain a newline.
        let others = git_ok(
            &self.workspace,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        for name in others.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let rel = Path::new(std::str::from_utf8(name).ok()?);
            copy_preserving_symlinks(&self.workspace.join(rel), &self.path.join(rel))?;
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
        Some(String::from_utf8(tree.stdout).ok()?.trim().to_string())
    }

    pub fn path(&self) -> &Path {
        &self.path
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

fn git(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
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

/// Copy `from` to `to`, creating parent directories. A symlink is recreated as
/// a symlink: a naive copy would dereference it and materialize the target's
/// bytes inside the child, which is both wrong and a way to smuggle a file
/// from outside the repo into the diff.
fn copy_preserving_symlinks(from: &Path, to: &Path) -> Option<()> {
    let meta = std::fs::symlink_metadata(from).ok()?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(from).ok()?;
        let _ = std::fs::remove_file(to);
        std::os::unix::fs::symlink(target, to).ok()?;
    } else if meta.is_file() {
        std::fs::copy(from, to).ok()?;
    }
    Some(())
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
        if !crate::shadow::git_available() {
            return None;
        }
        let tmp = tempfile::tempdir().ok()?;
        let dir = tmp.path();
        git_ok(dir, &["init", "-q", "-b", "main"])?;
        git_ok(dir, &["config", "user.email", "t@example.com"])?;
        git_ok(dir, &["config", "user.name", "t"])?;
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
    /// cannot see. `hotl undo`'s snapshot restores the working tree; a staging
    /// area silently mutated by a sub-agent is exactly what it cannot reverse.
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
        if !crate::shadow::git_available() {
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
        use std::os::unix::fs::PermissionsExt;
        let tmp = repo_or_skip!();
        let root = tmp.path();
        let locked = root.join("locked.txt");
        std::fs::write(&locked, "no\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&locked).is_ok() {
            return; // running as root: the copy cannot be made to fail here
        }

        assert!(Worktree::create(root, "01JTESTK").is_none());
        assert_eq!(worktree_count(root), 1, "a half-seeded worktree leaked");
        assert!(!root.join(WORKTREE_DIR).join("01JTESTK").exists());

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
