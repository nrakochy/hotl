//! Shadow-git snapshots (M3b): a per-session repo under the data dir that
//! snapshots the workspace tree around every mutating tool batch, so
//! `hotl undo` can restore the last pre-batch state.
//!
//! Deliberately **not** the user's repo: the shadow keeps its own git dir
//! (bare + explicit work-tree, the dotfiles pattern) and never touches the
//! workspace's `.git`. Contents are the user's own files — masking does not
//! apply (SECURITY.md §M3a rows). Uses the git CLI; hosts without git
//! degrade to no-snapshots (doctor warns).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Paths excluded from every snapshot, via `info/exclude`. Two purposes:
/// heavy build dirs that make snapshots slow and undo useless, and — added
/// for security-evaluation H-13 — secret-bearing files that must not be
/// duplicated into a second, history-retaining location. The shadow repo
/// mirrors the user's own files, but git history means a transient secret
/// persists in shadow objects after the workspace file is deleted or rotated,
/// so credentials are kept out of it entirely.
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
";

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
        let shadow = Self { git_dir, work_tree: work_tree.to_path_buf() };
        std::fs::write(shadow.git_dir.join("info/exclude"), EXCLUDES).ok()?;
        // Record the work tree so `hotl undo` (a later process) can find it.
        shadow.git(&["config", "hotl.worktree", &shadow.work_tree.display().to_string()])?;
        Some(shadow)
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

    /// Commit the current tree under `label`. Blocking (child process) —
    /// async callers wrap in spawn_blocking.
    pub fn snapshot(&self, label: &str) -> Option<String> {
        self.git(&["add", "-A", "."])?;
        self.git(&[
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
        let out = self.git(&["rev-parse", "HEAD"])?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// The most recent commit whose label starts with "pre " — the state
    /// before the agent's last mutating batch.
    pub fn latest_pre(&self) -> Option<(String, String)> {
        let out = self.git(&["log", "--format=%H %s"])?;
        String::from_utf8_lossy(&out.stdout).lines().find_map(|line| {
            let (hash, subject) = line.split_once(' ')?;
            subject
                .starts_with("pre ")
                .then(|| (hash.to_string(), subject.to_string()))
        })
    }

    /// Files that differ between the current tree and `hash` (what a
    /// restore would touch), then restore tracked files to that snapshot.
    /// Files created after the snapshot are reported but not deleted.
    pub fn restore(&self, hash: &str) -> Result<Vec<String>, String> {
        self.snapshot("checkpoint before undo")
            .ok_or("could not checkpoint the current tree")?;
        let diff = self
            .git(&["diff", "--name-only", hash, "HEAD"])
            .ok_or("git diff failed")?;
        let files: Vec<String> = String::from_utf8_lossy(&diff.stdout)
            .lines()
            .map(String::from)
            .collect();
        let out = self.git(&["checkout", "-q", hash, "--", "."]).ok_or("git checkout failed")?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        Ok(files)
    }

    pub fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    fn git(&self, args: &[&str]) -> Option<std::process::Output> {
        Command::new("git")
            .arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.work_tree.display()))
            .args(args)
            .current_dir(&self.work_tree)
            .output()
            .ok()
    }
}

/// Newest session shadow under the root (by directory mtime).
pub fn latest_session(shadow_root: &Path) -> Option<String> {
    let entries = std::fs::read_dir(shadow_root).ok()?;
    let mut dirs: Vec<(std::time::SystemTime, String)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".git")?.to_string();
            Some((e.metadata().ok()?.modified().ok()?, id))
        })
        .collect();
    dirs.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    dirs.into_iter().next().map(|(_, id)| id)
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
        shadow.snapshot("pre batch 1").expect("snapshot");

        let listing = std::process::Command::new("git")
            .arg(format!("--git-dir={}", root.path().join("SEC.git").display()))
            .args(["ls-files"])
            .output()
            .unwrap();
        let tracked = String::from_utf8_lossy(&listing.stdout);
        assert!(tracked.contains("main.rs"), "workspace source should snapshot");
        assert!(!tracked.contains(".env"), ".env must be excluded (H-13)");
        assert!(!tracked.contains("id_rsa"), "private keys must be excluded (H-13)");
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
        shadow.snapshot("pre batch 1").expect("pre snapshot");
        std::fs::write(work.path().join("a.txt"), "v2-broken").unwrap();
        std::fs::write(work.path().join("b.txt"), "new file").unwrap();
        shadow.snapshot("post batch 1").expect("post snapshot");

        // A later process finds the session and its work tree.
        assert_eq!(latest_session(root.path()).as_deref(), Some("SESSION1"));
        let reopened = Shadow::open(root.path(), "SESSION1").expect("open");
        assert_eq!(reopened.work_tree(), work.path());

        let (hash, label) = reopened.latest_pre().expect("pre commit");
        assert_eq!(label, "pre batch 1");
        let touched = reopened.restore(&hash).expect("restore");
        assert_eq!(std::fs::read_to_string(work.path().join("a.txt")).unwrap(), "v1");
        // Created-after files survive (reported, not deleted) — documented.
        assert!(work.path().join("b.txt").exists());
        assert!(touched.iter().any(|f| f == "a.txt"), "touched: {touched:?}");
    }
}
