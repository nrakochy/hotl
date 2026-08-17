//! Shadow-git snapshots (M3b): a per-session repo under the data dir that
//! captures the workspace tree at quiet windows — session start and after
//! each mutating tool batch — so `hotl undo` can restore the last clean
//! state. A capture whose staging overlapped a workspace mutation is
//! committed under a `tainted ` label and never restored; the
//! `hotl-mutations` marker gates undo to sessions where the agent actually
//! mutated something (0035).
//!
//! Deliberately **not** the user's repo: the shadow keeps its own git dir
//! (bare + explicit work-tree, the dotfiles pattern) and never touches the
//! workspace's `.git`. Contents are the user's own files — masking does not
//! apply (SECURITY.md §M3a rows). Uses the git CLI; hosts without git
//! degrade to no-snapshots (doctor warns).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Paths excluded from every snapshot, via `info/exclude`. Two purposes:
/// heavy build dirs that make snapshots slow and undo useless, and
/// secret-bearing files that must not be duplicated into a second,
/// history-retaining location. The shadow repo mirrors the user's own files,
/// but git history means a transient secret persists in shadow objects after
/// the workspace file is deleted or rotated, so credentials are kept out of it
/// entirely.
///
/// INVARIANT (best-effort — this is a blocklist, not a guarantee): the names
/// listed here never enter a snapshot. Enforced by
/// `secret_files_are_excluded_from_snapshots` and
/// `credential_shaped_files_never_enter_a_snapshot`. A blocklist cannot be
/// complete; a content-scanning or allowlist approach is tracked as tech debt.
/// Every session re-runs `Shadow::create`, which rewrites `info/exclude`, so
/// additions here reach existing installs on the next session with no migration.
const EXCLUDES: &str = "\
.git/
target/
node_modules/
.venv/
dist/
build/
.DS_Store
.env
.env.*
*.pem
*.key
id_rsa
id_dsa
id_ecdsa
id_ed25519
*.p12
*.pfx
.ssh/
.aws/
.npmrc
.pypirc
.netrc
secrets.*
*.secret
credentials
*.tfstate
*.tfstate.*
*.tfvars
.terraform/
kubeconfig
*.kubeconfig
.kube/
.git-credentials
*.jks
*.keystore
*.p8
*.ovpn
serviceaccount*.json
.gnupg/
*.kdbx
";

/// Snapshot label vocabulary (0035 decision 5): clean quiet-window captures
/// start with `state ` and are restore targets; captures whose staging
/// overlapped a workspace mutation start with `tainted ` — committed anyway
/// (they still warm the shadow index, so successive attempts converge), but
/// never restored. `checkpoint before undo` matches neither prefix.
pub const CLEAN_PREFIX: &str = "state ";
pub const TAINTED_PREFIX: &str = "tainted ";

/// The tainted form of a clean label: `state after batch 3` →
/// `tainted after batch 3`.
pub fn taint_label(label: &str) -> String {
    match label.strip_prefix(CLEAN_PREFIX) {
        Some(rest) => format!("{TAINTED_PREFIX}{rest}"),
        None => format!("{TAINTED_PREFIX}{label}"),
    }
}

pub struct Shadow {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

impl Shadow {
    /// Create the shadow repo for a session. `None` = git unavailable or
    /// init failed — the session simply runs without undo.
    pub fn create(shadow_root: &Path, session_id: &str, work_tree: &Path) -> Option<Self> {
        if !git_available() {
            return None;
        }
        let git_dir = shadow_root.join(format!("{session_id}.git"));
        std::fs::create_dir_all(&git_dir).ok()?;
        let init = Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&git_dir)
            .output()
            .ok()?;
        if !init.status.success() {
            return None;
        }
        let shadow = Self {
            git_dir,
            work_tree: work_tree.to_path_buf(),
        };
        std::fs::write(shadow.git_dir.join("info/exclude"), EXCLUDES).ok()?;
        // Record the work tree so `hotl undo` (a later process) can find it.
        // `git_ok`, not `git`: a failed config write means `Shadow::open` later
        // cannot find the work tree, so fall back to no-snapshots rather than
        // pretend this session has undo.
        shadow.git_ok(&[
            "config",
            "hotl.worktree",
            &shadow.work_tree.display().to_string(),
        ])?;
        // Cheap-snapshot levers (0035 decision 10), best-effort: each only
        // shrinks staging latency, never gates undo.
        for kv in [
            ["config", "core.untrackedCache", "true"],
            ["config", "core.preloadIndex", "true"],
            ["config", "feature.manyFiles", "true"],
        ] {
            let _ = shadow.git(&kv);
        }
        shadow.seed_alternates();
        Some(shadow)
    }

    /// Borrow the workspace repo's object store via `objects/info/alternates`
    /// (0035 decision 10). `git add` still hashes every file; blobs already in
    /// the workspace history just skip the object *write*. Best-effort.
    fn seed_alternates(&self) {
        let Some(out) = Command::new("git")
            .arg("-C")
            .arg(&self.work_tree)
            .args(["rev-parse", "--git-dir"])
            .output()
            .ok()
            .filter(|o| o.status.success())
        else {
            return;
        };
        let workspace_git = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        let workspace_git = if workspace_git.is_absolute() {
            workspace_git
        } else {
            self.work_tree.join(workspace_git)
        };
        // Absolute, symlink-free: the alternates file is read from the shadow
        // git dir, so a relative workspace path would resolve wrong.
        let Ok(odb) = dunce::canonicalize(workspace_git.join("objects")) else {
            return;
        };
        let _ = std::fs::write(
            self.git_dir.join("objects/info/alternates"),
            format!("{}\n", odb.display()),
        );
    }

    /// Open an existing shadow repo, reading its recorded work tree.
    pub fn open(shadow_root: &Path, session_id: &str) -> Option<Self> {
        let git_dir = shadow_root.join(format!("{session_id}.git"));
        if !git_dir.is_dir() {
            return None;
        }
        let out = Command::new("git")
            .arg(format!("--git-dir={}", git_dir.display()))
            .args(["config", "hotl.worktree"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let work_tree = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        Some(Self { git_dir, work_tree })
    }

    /// Stage the current tree (`git add -A .`) into the shadow index.
    /// Blocking (child process); the expensive half of a snapshot.
    pub fn stage(&self) -> Option<()> {
        self.git_ok(&["add", "-A", "."]).map(|_| ())
    }

    /// Commit whatever `stage` staged under `label`. Reads only the index —
    /// workspace mutations after `stage` returned cannot tear the capture.
    pub fn commit(&self, label: &str) -> Option<String> {
        self.git_ok(&[
            "-c",
            "user.name=hotl",
            "-c",
            "user.email=hotl@localhost",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            label,
        ])?;
        let out = self.git_ok(&["rev-parse", "HEAD"])?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Commit the current tree under `label` — `stage` + `commit`.
    ///
    /// INVARIANT: `Some(hash)` means the commit reflects the workspace as it
    /// was read. Every git step's exit status is checked, so a failed `add`
    /// can no longer be followed by a `commit --allow-empty` that silently
    /// records the *previous* tree. Enforced by
    /// `snapshot_fails_when_the_index_is_locked`.
    pub fn snapshot(&self, label: &str) -> Option<String> {
        self.stage()?;
        self.commit(label)
    }

    /// The most recent clean snapshot — newest commit labeled `state …`.
    /// Falls back to the legacy `pre ` labels older on-disk shadows carry
    /// (retention ages them out). `tainted …` and `checkpoint before undo`
    /// match neither prefix.
    pub fn latest_clean(&self) -> Option<(String, String)> {
        let out = self.git(&["log", "--format=%H %s"])?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let newest = |prefix: &str| {
            text.lines().find_map(|line| {
                let (hash, subject) = line.split_once(' ')?;
                subject
                    .starts_with(prefix)
                    .then(|| (hash.to_string(), subject.to_string()))
            })
        };
        newest(CLEAN_PREFIX).or_else(|| newest("pre "))
    }

    /// Record that the agent mutated the workspace this session. Undo refuses
    /// without the marker: a session-start-only shadow must not restore over
    /// user edits the agent never touched (0035 decision 6). Best-effort —
    /// a failed write degrades to undo refusing, the safe direction.
    pub fn mark_mutations(&self) {
        let _ = std::fs::write(self.git_dir.join("hotl-mutations"), "");
    }

    pub fn has_mutations(&self) -> bool {
        self.git_dir.join("hotl-mutations").exists()
    }

    /// Files that differ between the current tree and `hash` (what a restore
    /// touched), restoring the workspace to that snapshot.
    ///
    /// INVARIANT: after `restore`, every non-excluded path matches `hash` —
    /// including files created after the snapshot, which are deleted, not kept.
    /// `git clean` cannot do this: the checkpoint commit taken below tracks
    /// those files, so they are never untracked. Excluded paths (secrets,
    /// build output) are never tracked, never in the diff, never touched.
    /// Enforced by `snapshot_and_undo_roundtrip` and
    /// `undo_never_touches_excluded_files`.
    ///
    /// Reversible: the checkpoint commit still holds everything removed here.
    pub fn restore(&self, hash: &str) -> Result<Vec<String>, String> {
        self.snapshot("checkpoint before undo").ok_or(
            "could not checkpoint the current tree — refusing to restore over unsaved work",
        )?;
        let diff = self
            .git_ok(&["diff", "--name-only", hash, "HEAD"])
            .ok_or("git diff failed")?;
        let files: Vec<String> = String::from_utf8_lossy(&diff.stdout)
            .lines()
            .map(String::from)
            .collect();
        // Paths that exist only *after* the snapshot. `git checkout <hash> -- .`
        // will not remove them (they are not in `<hash>`), so name them.
        let added = self
            .git_ok(&["diff", "--name-only", "--diff-filter=A", hash, "HEAD"])
            .ok_or("git diff failed")?;
        let added: Vec<String> = String::from_utf8_lossy(&added.stdout)
            .lines()
            .map(String::from)
            .collect();

        let out = self
            .git(&["checkout", "-q", hash, "--", "."])
            .ok_or("git checkout failed")?;
        if !out.status.success() {
            let mut err = String::from_utf8_lossy(&out.stderr).into_owned();
            if self.git_dir.join("objects/info/alternates").exists() {
                // Accepted risk (0035 decision 10): the shadow borrows objects
                // from the workspace odb; a history rewrite + gc there can
                // prune what this snapshot still references.
                err.push_str(
                    "\n(the shadow borrows objects from the workspace repo; a history \
                     rewrite plus gc there can prune them, making this snapshot unrecoverable)",
                );
            }
            return Err(err);
        }
        if !added.is_empty() {
            // `git rm` with an empty pathspec is a fatal error, hence the guard.
            let mut args: Vec<&str> = vec!["rm", "-q", "-f", "--ignore-unmatch", "--"];
            args.extend(added.iter().map(String::as_str));
            let rm = self.git(&args).ok_or("git rm failed")?;
            if !rm.status.success() {
                return Err(String::from_utf8_lossy(&rm.stderr).into_owned());
            }
        }
        Ok(files)
    }

    pub fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    /// `git`, but a non-zero exit is a failure. `git` alone only reports
    /// whether the *process spawned* — the distinction that made T2-15b silent.
    fn git_ok(&self, args: &[&str]) -> Option<std::process::Output> {
        self.git(args).filter(|o| o.status.success())
    }

    fn git(&self, args: &[&str]) -> Option<std::process::Output> {
        Command::new("git")
            // Global flag, before the subcommand: never take optional locks,
            // so a concurrent `git status` on the same tree can't contend.
            .arg("--no-optional-locks")
            .arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.work_tree.display()))
            .args(args)
            .current_dir(&self.work_tree)
            .output()
            .ok()
    }
}

/// The newest session shadow under the root, by **session id**.
///
/// INVARIANT: selection never depends on filesystem timestamps. Ids are ULIDs,
/// which sort lexicographically in creation order, so the maximum id is the
/// most recently started session — where directory mtime is whatever last
/// touched the tree, which is not the same thing (T2-15c). Enforced by
/// `latest_session_picks_the_newest_id_not_the_newest_directory`.
pub fn latest_session(shadow_root: &Path) -> Option<String> {
    std::fs::read_dir(shadow_root)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.strip_suffix(".git").map(str::to_string)
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_files_are_excluded_from_snapshots() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(work.path().join(".env"), "SECRET=leaked-value-9").unwrap();
        std::fs::create_dir_all(work.path().join(".ssh")).unwrap();
        std::fs::write(work.path().join(".ssh/id_rsa"), "PRIVATE KEY").unwrap();

        let shadow = Shadow::create(root.path(), "SEC", work.path()).expect("create");
        shadow.snapshot("state after batch 1").expect("snapshot");

        let listing = std::process::Command::new("git")
            .arg(format!(
                "--git-dir={}",
                root.path().join("SEC.git").display()
            ))
            .args(["ls-files"])
            .output()
            .unwrap();
        let tracked = String::from_utf8_lossy(&listing.stdout);
        assert!(
            tracked.contains("main.rs"),
            "workspace source should snapshot"
        );
        assert!(!tracked.contains(".env"), ".env must be excluded (H-13)");
        assert!(
            !tracked.contains("id_rsa"),
            "private keys must be excluded (H-13)"
        );
    }

    #[test]
    fn credential_shaped_files_never_enter_a_snapshot() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("main.rs"), "fn main() {}").unwrap();

        // Every one of these is a routine file in a normal repo, and every one
        // would persist in shadow git objects after the user rotated it (T2-15d).
        let files = [
            "terraform.tfstate",
            "terraform.tfstate.backup",
            "kubeconfig",
            ".git-credentials",
            "release.jks",
            "app.keystore",
            "client.ovpn",
            "serviceaccount-prod.json",
            "prod.tfvars",
        ];
        for f in files {
            std::fs::write(work.path().join(f), "CREDENTIAL").unwrap();
        }
        std::fs::create_dir_all(work.path().join(".kube")).unwrap();
        std::fs::write(work.path().join(".kube/config"), "CREDENTIAL").unwrap();

        let shadow = Shadow::create(root.path(), "SEC2", work.path()).expect("create");
        shadow.snapshot("state after batch 1").expect("snapshot");

        let listing = std::process::Command::new("git")
            .arg(format!(
                "--git-dir={}",
                root.path().join("SEC2.git").display()
            ))
            .args(["ls-files"])
            .output()
            .unwrap();
        let tracked = String::from_utf8_lossy(&listing.stdout);
        assert!(
            tracked.contains("main.rs"),
            "workspace source should snapshot"
        );
        for f in files {
            assert!(!tracked.contains(f), "{f} must be excluded (T2-15d)");
        }
        assert!(
            !tracked.contains(".kube/"),
            ".kube/ must be excluded (T2-15d)"
        );
    }

    #[test]
    fn snapshot_and_undo_roundtrip() {
        if !git_available() {
            eprintln!("git not available — skipping shadow test");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("a.txt"), "v1").unwrap();

        let shadow = Shadow::create(root.path(), "SESSION1", work.path()).expect("create");
        shadow
            .snapshot("state after batch 1")
            .expect("clean snapshot");
        std::fs::write(work.path().join("a.txt"), "v2-broken").unwrap();
        std::fs::write(work.path().join("b.txt"), "new file").unwrap();
        std::fs::create_dir_all(work.path().join("sub")).unwrap();
        std::fs::write(work.path().join("sub/c.txt"), "also new").unwrap();
        // A capture that raced a mutation: committed, but never a restore target.
        shadow
            .snapshot("tainted after batch 2")
            .expect("tainted snapshot");

        // A later process finds the session's work tree.
        let reopened = Shadow::open(root.path(), "SESSION1").expect("open");
        assert_eq!(reopened.work_tree(), work.path());

        let (hash, label) = reopened.latest_clean().expect("clean commit");
        assert_eq!(label, "state after batch 1");
        let touched = reopened.restore(&hash).expect("restore");

        assert_eq!(
            std::fs::read_to_string(work.path().join("a.txt")).unwrap(),
            "v1",
            "modified files are restored"
        );
        assert!(
            !work.path().join("b.txt").exists(),
            "undo must remove files created after the snapshot (T2-15)"
        );
        assert!(
            !work.path().join("sub/c.txt").exists(),
            "including files in directories created after the snapshot"
        );
        assert!(touched.iter().any(|f| f == "a.txt"), "touched: {touched:?}");
        assert!(touched.iter().any(|f| f == "b.txt"), "touched: {touched:?}");
    }

    #[test]
    fn latest_session_picks_the_newest_id_not_the_newest_directory() {
        let root = tempfile::tempdir().unwrap();
        // ULIDs sort in creation order. Create them in an order where the
        // most-recently-touched directory is NOT the newest session.
        for id in [
            "01HZZZZZZZZZZZZZZZZZZZZZZZ",
            "01HAAAAAAAAAAAAAAAAAAAAAAA",
            "01HMMMMMMMMMMMMMMMMMMMMMMM",
        ] {
            std::fs::create_dir_all(root.path().join(format!("{id}.git"))).unwrap();
        }
        // The last-created dir (…MMM…) has the newest mtime; the newest session
        // is …ZZZ…, created first.
        assert_eq!(
            latest_session(root.path()).as_deref(),
            Some("01HZZZZZZZZZZZZZZZZZZZZZZZ"),
            "undo must target the newest session, not the most recently touched directory"
        );
    }

    #[test]
    fn snapshot_fails_when_the_index_is_locked() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("a.txt"), "v1").unwrap();
        let shadow = Shadow::create(root.path(), "LOCKED", work.path()).expect("create");
        assert!(
            shadow.snapshot("state after batch 1").is_some(),
            "baseline works"
        );

        // Exactly what a concurrent snapshot or a crashed git leaves behind.
        std::fs::write(root.path().join("LOCKED.git/index.lock"), "").unwrap();
        std::fs::write(work.path().join("a.txt"), "v2").unwrap();

        assert!(
            shadow.stage().is_none(),
            "a locked index must fail the stage, not silently keep the old tree"
        );
        assert!(
            shadow.snapshot("state after batch 2").is_none(),
            "a failed `git add` must not yield a snapshot of the previous tree"
        );
    }

    #[test]
    fn undo_never_touches_excluded_files() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("a.txt"), "v1").unwrap();

        let shadow = Shadow::create(root.path(), "SESSION2", work.path()).expect("create");
        shadow
            .snapshot("state after batch 1")
            .expect("clean snapshot");

        // Created after the snapshot, but excluded — an undo must not delete the
        // user's credentials or their build output.
        std::fs::write(work.path().join(".env"), "SECRET=x").unwrap();
        std::fs::create_dir_all(work.path().join("target")).unwrap();
        std::fs::write(work.path().join("target/out"), "build").unwrap();
        std::fs::write(work.path().join("a.txt"), "v2").unwrap();
        shadow
            .snapshot("tainted after batch 2")
            .expect("tainted snapshot");

        let (hash, _) = shadow.latest_clean().expect("clean commit");
        shadow.restore(&hash).expect("restore");
        assert!(
            work.path().join(".env").exists(),
            "excluded secrets survive undo"
        );
        assert!(
            work.path().join("target/out").exists(),
            "excluded build output survives undo"
        );
    }

    #[test]
    fn latest_clean_picks_newest_state_and_skips_tainted() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("a.txt"), "v1").unwrap();
        let shadow = Shadow::create(root.path(), "LABELS", work.path()).expect("create");
        shadow.snapshot("state session start").unwrap();
        shadow.snapshot("state after batch 1").unwrap();
        shadow.snapshot("tainted after batch 2").unwrap();
        shadow.snapshot("checkpoint before undo").unwrap();

        let (_, label) = shadow.latest_clean().expect("clean commit");
        assert_eq!(
            label, "state after batch 1",
            "newest clean wins; tainted and checkpoint labels never do"
        );
    }

    #[test]
    fn latest_clean_falls_back_to_legacy_pre_labels() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("a.txt"), "v1").unwrap();
        let shadow = Shadow::create(root.path(), "LEGACY", work.path()).expect("create");
        // What a pre-0035 shadow left on disk; retention ages these out.
        shadow.snapshot("pre batch 1").unwrap();
        shadow.snapshot("post batch 1").unwrap();

        let (_, label) = shadow.latest_clean().expect("legacy fallback");
        assert_eq!(label, "pre batch 1");
    }

    #[test]
    fn mutation_marker_roundtrips_via_open() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let shadow = Shadow::create(root.path(), "MARKER", work.path()).expect("create");
        assert!(!shadow.has_mutations(), "fresh sessions start unmarked");
        shadow.mark_mutations();
        assert!(shadow.has_mutations());

        let reopened = Shadow::open(root.path(), "MARKER").expect("open");
        assert!(
            reopened.has_mutations(),
            "undo (a separate process) must see the marker"
        );
    }

    #[test]
    fn create_on_a_git_workspace_seeds_alternates_and_roundtrips() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(work.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(work.path().join("a.txt"), "v1").unwrap();

        let shadow = Shadow::create(root.path(), "ALT", work.path()).expect("create");
        let alternates = root.path().join("ALT.git/objects/info/alternates");
        let listed = std::fs::read_to_string(&alternates).expect("alternates written");
        let odb = dunce::canonicalize(work.path().join(".git/objects")).unwrap();
        assert_eq!(listed.trim(), odb.display().to_string());

        // Borrowed odb or not, the snapshot/restore cycle must still work.
        shadow.snapshot("state after batch 1").expect("snapshot");
        std::fs::write(work.path().join("a.txt"), "v2").unwrap();
        let (hash, _) = shadow.latest_clean().unwrap();
        shadow.restore(&hash).expect("restore");
        assert_eq!(
            std::fs::read_to_string(work.path().join("a.txt")).unwrap(),
            "v1"
        );
    }
}
