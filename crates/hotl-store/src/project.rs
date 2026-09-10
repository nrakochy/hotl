//! Project identity — the key a plan artifact is filed under (0056 T2).
//!
//! The origin remote, when there is one: two worktrees of the same repo, a
//! clone on another machine, and a `cd` into a subdirectory all name the same
//! project, which is the whole point of filing a plan by project rather than
//! by path. Without a remote (or without git) the canonical cwd is the
//! fallback, and two unrelated checkouts stay unrelated.

use std::path::Path;

/// The git read's deadline. A wedged repo (cold NFS, giant index) must not
/// hold session start hostage; expiry falls back to the cwd, which is a
/// correct-if-narrower answer, never a wrong one.
const GIT_CAP: std::time::Duration = std::time::Duration::from_millis(1500);

/// Stable hex id for the project rooted at `cwd`.
pub fn id(cwd: &Path) -> String {
    let key = origin_url(cwd).unwrap_or_else(|| {
        dunce::canonicalize(cwd)
            .unwrap_or_else(|_| cwd.to_path_buf())
            .to_string_lossy()
            .into_owned()
    });
    format!("{:016x}", fnv1a64(key.as_bytes()))
}

/// `git remote get-url origin`, trimmed. `None` on any failure — not a repo,
/// no origin, git missing, or the cap expiring.
fn origin_url(cwd: &Path) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut cmd = std::process::Command::new("git");
    cmd.args(["--no-optional-locks", "remote", "get-url", "origin"])
        .current_dir(cwd)
        // An inherited GIT_DIR would name some other repo entirely — the
        // same trap `agent::run_capped` guards, and it has corrupted one.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    // The output is read on a thread, never by polling a piped child: a
    // `try_wait` loop deadlocks once the child outgrows the pipe buffer. On
    // timeout the child is disowned, not killed.
    std::thread::spawn(move || {
        let _ = tx.send(cmd.output());
    });
    match rx.recv_timeout(GIT_CAP) {
        Ok(Ok(o)) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (!s.is_empty()).then_some(s)
        }
        _ => None,
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?} failed");
    }

    #[test]
    fn project_id_is_stable_for_remote_and_falls_back_to_cwd() {
        // No git at all: the id is the canonical cwd's, and it is stable.
        let plain = tempfile::tempdir().expect("tempdir");
        let a = id(plain.path());
        assert_eq!(a, id(plain.path()));
        assert_eq!(a.len(), 16, "fnv1a64 hex: {a}");

        // Two different directories are two different projects.
        let other = tempfile::tempdir().expect("tempdir");
        assert_ne!(a, id(other.path()));

        // Two checkouts sharing an origin are ONE project — the property the
        // cwd fallback cannot express, and the reason this reads git at all.
        let one = tempfile::tempdir().expect("tempdir");
        let two = tempfile::tempdir().expect("tempdir");
        for d in [one.path(), two.path()] {
            git(d, &["init", "-q"]);
            git(
                d,
                &["remote", "add", "origin", "https://example.invalid/x.git"],
            );
        }
        assert_eq!(id(one.path()), id(two.path()));
        assert_ne!(id(one.path()), a, "the remote id is not the cwd id");
    }
}
