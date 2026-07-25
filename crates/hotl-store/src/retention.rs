//! Retention / GC (owed since M2/M3b): bound the growth of the append-only
//! stores by age and count. Prunes a whole session as a unit — its `.jsonl`
//! log, its `.blobs/` (evicted tool results), and its `.git` shadow snapshot
//! repo — so nothing is left half-deleted. Never touches the workspace, never
//! rewrites a file in place (append-only stays append-only; deletion is the
//! only GC, per the retention row in SECURITY.md/RELIABILITY.md).
//!
//! **Lineage-aware** (T2-5): resume is fork, so a resumed conversation is a
//! chain whose ancestors are old by definition. GC never prunes a session that
//! a retained session descends from.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// What to keep. A session is pruned if it exceeds *either* limit. Both `None`
/// = keep everything (the safe default; GC is opt-in).
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    /// Delete sessions older than this.
    pub max_age: Option<Duration>,
    /// Keep at most this many (the newest); delete the rest.
    pub max_sessions: Option<usize>,
}

impl RetentionPolicy {
    pub fn is_noop(&self) -> bool {
        self.max_age.is_none() && self.max_sessions.is_none()
    }
}

/// A pruned session and the bytes it freed.
#[derive(Debug)]
pub struct PrunedSession {
    pub id: String,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub pruned: Vec<PrunedSession>,
    /// Sessions the age/count policy selected but lineage rescued: a retained
    /// session descends from them, so deleting them would destroy a live
    /// conversation (T2-5).
    pub protected: Vec<String>,
    pub dry_run: bool,
}

impl GcReport {
    pub fn bytes_freed(&self) -> u64 {
        self.pruned.iter().map(|p| p.bytes).sum()
    }
}

/// Prune sessions under `data_dir` (which holds `sessions/`, `shadow/`) per
/// `policy`. `dry_run` reports what *would* go without deleting.
///
/// INVARIANT: a session that any retained session descends from is never
/// pruned. Resume is fork, so a resumed conversation's ancestors are old by
/// definition and an age/count policy alone would delete the history the user
/// considers current. Enforced by `gc_never_prunes_an_ancestor_of_a_retained_session`.
pub fn gc(data_dir: &Path, policy: &RetentionPolicy, dry_run: bool) -> GcReport {
    let sessions_dir = data_dir.join("sessions");
    let shadow_dir = data_dir.join("shadow");
    // (id, log path, modified), newest first.
    let sessions = crate::list_sessions(&sessions_dir);
    let now = SystemTime::now();

    let mut report = GcReport {
        dry_run,
        ..Default::default()
    };

    // Pass 1 — what age/count alone would take.
    let over_policy: Vec<bool> = sessions
        .iter()
        .enumerate()
        .map(|(idx, (_, _, modified))| {
            let too_old = policy.max_age.is_some_and(|max| {
                now.duration_since(*modified)
                    .map(|age| age > max)
                    .unwrap_or(false)
            });
            let over_count = policy.max_sessions.is_some_and(|keep| idx >= keep);
            too_old || over_count
        })
        .collect();

    // Pass 2 — the ancestor closure of everything pass 1 keeps. `ancestor_ids`
    // walks the *whole* chain from each retained session, so a rescued
    // ancestor's own ancestors are already in the set; no fixpoint is needed.
    let mut protected: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ((id, _, _), over) in sessions.iter().zip(&over_policy) {
        if !over {
            protected.extend(crate::ancestor_ids(&sessions_dir, id));
        }
    }

    for ((id, log_path, _), over) in sessions.iter().zip(&over_policy) {
        if !over {
            continue;
        }
        if protected.contains(id) {
            report.protected.push(id.clone());
            continue;
        }
        let targets = session_paths(log_path, &shadow_dir, id);
        let bytes = targets.iter().map(|p| dir_or_file_size(p)).sum();
        if !dry_run {
            for p in &targets {
                remove(p);
            }
        }
        report.pruned.push(PrunedSession {
            id: id.clone(),
            bytes,
        });
    }
    report
}

/// The three on-disk artifacts of one session: log, blob dir, shadow repo.
fn session_paths(log_path: &Path, shadow_dir: &Path, id: &str) -> Vec<PathBuf> {
    let mut v = vec![log_path.to_path_buf()];
    if let Some(stem) = log_path.file_stem().and_then(|s| s.to_str()) {
        v.push(log_path.with_file_name(format!("{stem}.blobs")));
    }
    v.push(shadow_dir.join(format!("{id}.git")));
    v
}

fn dir_or_file_size(p: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if meta.is_dir() {
        std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| dir_or_file_size(&e.path())).sum())
            .unwrap_or(0)
    } else {
        meta.len()
    }
}

fn remove(p: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return;
    };
    let _ = if meta.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Masker, SessionLog};

    fn make_session(sessions: &Path, shadow: &Path) -> String {
        let log = SessionLog::create(sessions, "m", None, Masker::empty(), 0).unwrap();
        let id = log.session_id.clone();
        // Give it a blob and a shadow dir so pruning covers all three.
        log.write_blob("t1", "big result").unwrap();
        std::fs::create_dir_all(shadow.join(format!("{id}.git"))).unwrap();
        std::fs::write(shadow.join(format!("{id}.git/HEAD")), "ref").unwrap();
        id
    }

    /// A parent→child chain of `depth` sessions; returns the ids oldest-first.
    fn make_chain(sessions: &Path, shadow: &Path, depth: usize) -> Vec<String> {
        let mut ids = Vec::new();
        let mut parent: Option<String> = None;
        for _ in 0..depth {
            let log =
                SessionLog::create(sessions, "m", parent.clone(), Masker::empty(), 0).unwrap();
            let id = log.session_id.clone();
            log.write_blob("t1", "big result").unwrap();
            std::fs::create_dir_all(shadow.join(format!("{id}.git"))).unwrap();
            parent = Some(id.clone());
            ids.push(id);
        }
        ids
    }

    #[test]
    fn gc_never_prunes_an_ancestor_of_a_retained_session() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let sessions = data.join("sessions");
        let shadow = data.join("shadow");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&shadow).unwrap();

        // One conversation, resumed twice → a 3-deep chain. Keep 1.
        let chain = make_chain(&sessions, &shadow, 3);
        let policy = RetentionPolicy {
            max_sessions: Some(1),
            max_age: None,
        };
        let report = gc(data, &policy, false);

        assert!(
            report.pruned.is_empty(),
            "every session is an ancestor of the one retained: {:?}",
            report.pruned.iter().map(|p| &p.id).collect::<Vec<_>>()
        );
        assert_eq!(report.protected.len(), 2, "two ancestors rescued");
        for id in &chain {
            assert!(
                sessions.join(format!("{id}.jsonl")).exists(),
                "{id} must survive — a live conversation depends on it"
            );
        }
    }

    #[test]
    fn gc_still_prunes_a_session_nothing_descends_from() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let sessions = data.join("sessions");
        let shadow = data.join("shadow");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&shadow).unwrap();

        let orphan = make_chain(&sessions, &shadow, 1).pop().unwrap();
        let chain = make_chain(&sessions, &shadow, 2); // newer than the orphan
        let newest = chain.last().unwrap().clone();

        let policy = RetentionPolicy {
            max_sessions: Some(1),
            max_age: None,
        };
        let report = gc(data, &policy, false);

        let pruned: Vec<&str> = report.pruned.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            pruned,
            [orphan.as_str()],
            "only the standalone session goes"
        );
        assert!(sessions.join(format!("{newest}.jsonl")).exists());
        assert!(!sessions.join(format!("{orphan}.jsonl")).exists());
    }

    #[test]
    fn count_cap_prunes_oldest_and_all_three_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let sessions = data.join("sessions");
        let shadow = data.join("shadow");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&shadow).unwrap();

        let ids: Vec<String> = (0..3)
            .map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(10));
                make_session(&sessions, &shadow)
            })
            .collect();

        // Keep the newest 1 → the two oldest are pruned.
        let policy = RetentionPolicy {
            max_sessions: Some(1),
            max_age: None,
        };
        let report = gc(data, &policy, false);
        assert_eq!(report.pruned.len(), 2, "two oldest pruned");
        // The newest survives with its blobs + shadow; the oldest are gone.
        let newest = ids.last().unwrap();
        assert!(sessions.join(format!("{newest}.jsonl")).exists());
        for pruned in &report.pruned {
            assert!(!sessions.join(format!("{}.jsonl", pruned.id)).exists());
            assert!(
                !sessions.join(format!("{}.blobs", pruned.id)).exists(),
                "blob dir pruned"
            );
            assert!(
                !shadow.join(format!("{}.git", pruned.id)).exists(),
                "shadow repo pruned"
            );
        }
    }

    #[test]
    fn dry_run_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        std::fs::create_dir_all(data.join("sessions")).unwrap();
        std::fs::create_dir_all(data.join("shadow")).unwrap();
        let id = make_session(&data.join("sessions"), &data.join("shadow"));
        let policy = RetentionPolicy {
            max_sessions: Some(0),
            max_age: None,
        };
        let report = gc(data, &policy, true);
        assert_eq!(report.pruned.len(), 1);
        assert!(report.dry_run);
        assert!(
            data.join(format!("sessions/{id}.jsonl")).exists(),
            "dry-run kept the file"
        );
    }

    #[test]
    fn noop_policy_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        std::fs::create_dir_all(data.join("sessions")).unwrap();
        std::fs::create_dir_all(data.join("shadow")).unwrap();
        make_session(&data.join("sessions"), &data.join("shadow"));
        assert!(RetentionPolicy::default().is_noop());
        let report = gc(data, &RetentionPolicy::default(), false);
        assert!(report.pruned.is_empty());
    }
}
