//! `hotl gc` — prune old session logs, blob dirs, and shadow snapshot repos
//! (retention/GC). Also sweeps dead backgrounded-session sockets. Policy comes
//! from `[retention]` in config.toml (see `crate::config`), overridable by
//! flags; with no policy configured and no flags, GC is a no-op that says so.

use std::time::Duration;

use hotl_store::retention::{gc, RetentionPolicy};

/// `hotl gc [--dry-run] [--days N] [--keep N]`.
pub fn gc_main(args: &[String]) -> i32 {
    let dry_run = args.iter().any(|a| a == "--dry-run" || a == "-n");
    let flag_days = flag_value(args, "--days").and_then(|v| v.parse::<u64>().ok());
    let flag_keep = flag_value(args, "--keep").and_then(|v| v.parse::<usize>().ok());

    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    let policy = RetentionPolicy {
        max_age: flag_days
            .or(cfg.retention.max_age_days)
            .map(|d| Duration::from_secs(d * 86_400)),
        max_sessions: flag_keep.or(cfg.retention.max_sessions),
    };

    if policy.is_noop() {
        println!(
            "no retention policy set — nothing pruned.\n\
             set [retention] max_age_days / max_sessions in config.toml, or pass --days N / --keep N."
        );
        return 0;
    }

    let data_dir = data_dir();
    let report = gc(&data_dir, &policy, dry_run);
    let sockets = sweep_dead_sockets(dry_run);

    let verb = if dry_run { "would prune" } else { "pruned" };
    if report.pruned.is_empty() && sockets == 0 {
        println!("nothing to prune — the stores are within the retention policy.");
    } else {
        println!(
            "{verb} {} session(s), freeing {} — plus {sockets} dead socket(s).",
            report.pruned.len(),
            human_bytes(report.bytes_freed()),
        );
        if dry_run {
            for p in &report.pruned {
                println!("  {} ({})", p.id, human_bytes(p.bytes));
            }
            println!("(dry run — nothing was deleted; re-run without --dry-run)");
        }
    }
    0
}

/// Remove `run/<id>.sock` files whose server is gone (connect refused). The
/// live server removes its own socket on exit; this catches crashed ones.
fn sweep_dead_sockets(dry_run: bool) -> usize {
    let run = crate::session_server::run_dir();
    let Ok(entries) = std::fs::read_dir(&run) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "sock") {
            continue;
        }
        if std::os::unix::net::UnixStream::connect(&p).is_err() {
            if !dry_run {
                let _ = std::fs::remove_file(&p);
                let _ = std::fs::remove_file(p.with_extension("log"));
            }
            removed += 1;
        }
    }
    removed
}

/// Automatic prune on startup when a policy is configured (best-effort, quiet).
/// Called off the hot path; a configured policy means the owner opted in.
pub fn auto_gc(config_dir: &std::path::Path) {
    let cfg = crate::config::Config::load(config_dir);
    let policy = RetentionPolicy {
        max_age: cfg
            .retention
            .max_age_days
            .map(|d| Duration::from_secs(d * 86_400)),
        max_sessions: cfg.retention.max_sessions,
    };
    if policy.is_noop() {
        return;
    }
    let _ = gc(&data_dir(), &policy, false);
    let _ = sweep_dead_sockets(false);
}

fn data_dir() -> std::path::PathBuf {
    crate::session_server::run_dir()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn human_bytes(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
