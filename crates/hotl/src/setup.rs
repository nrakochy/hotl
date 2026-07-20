//! First-run wizard (`hotl setup`) and update check (`hotl update`) — MD.
//!
//! `setup` writes the default config the "defaults are the safety design"
//! belief depends on, but **never silently**: it's an explicit subcommand and
//! it refuses to overwrite an existing file without `--force`. Bare `hotl`
//! prints a one-line hint on first run rather than writing anything itself.

use std::path::Path;

/// The shipped default allow-rules (design-docs/default-policy.md). Read-only
/// conveniences only; everything that runs code or writes files is commented
/// out for the owner to enable deliberately.
pub const DEFAULT_PERMISSIONS: &str = "\
# ~/.config/hotl/permissions.toml — hotl allow-rules (see docs/user/).
# Rules auto-approve a class of tool call. They are TRUST GRANTS, not scopes.
# Anything not matched here still asks. Delete this file to make all ask.
# Protected paths (ssh/creds/build.rs/git hooks/…) ALWAYS ask, rule or not.

# --- read-only inspection: safe to auto-allow ---
[[allow]]
tool = \"bash\"
prefix = \"ls \"

[[allow]]
tool = \"bash\"
prefix = \"cat \"

[[allow]]
tool = \"bash\"
prefix = \"git status\"

[[allow]]
tool = \"bash\"
prefix = \"git diff\"

[[allow]]
tool = \"bash\"
prefix = \"git log\"

# --- build/test in THIS project (runs code — opt in by uncommenting) ---
# [[allow]]
# tool = \"bash\"
# prefix = \"cargo test\"

# --- editing your own source tree (scope to what you want edited freely) ---
# [[allow]]
# tool = \"edit\"
# path_prefix = \"src/\"
";

/// `hotl setup [--force]`: write the default config, reporting each file.
pub fn setup_main(config_dir: &Path, force: bool) -> i32 {
    if let Err(e) = std::fs::create_dir_all(config_dir) {
        eprintln!("hotl: could not create {}: {e}", config_dir.display());
        return 1;
    }
    let perms = config_dir.join("permissions.toml");
    match write_if_absent(&perms, DEFAULT_PERMISSIONS, force) {
        Wrote::Created => println!("wrote {}", perms.display()),
        Wrote::Exists => println!("kept existing {} (use --force to overwrite)", perms.display()),
        Wrote::Failed(e) => {
            eprintln!("hotl: could not write {}: {e}", perms.display());
            return 1;
        }
    }
    println!(
        "config dir: {}\nnext: set a provider (HOTL_MODEL + a key, or HOTL_OPENAI_BASE_URL), \
         then `hotl doctor` to verify.",
        config_dir.display()
    );
    0
}

enum Wrote {
    Created,
    Exists,
    Failed(std::io::Error),
}

fn write_if_absent(path: &Path, content: &str, force: bool) -> Wrote {
    if path.exists() && !force {
        return Wrote::Exists;
    }
    match std::fs::write(path, content) {
        Ok(()) => Wrote::Created,
        Err(e) => Wrote::Failed(e),
    }
}

/// One-line first-run hint printed by bare `hotl` when no config exists.
/// Never writes anything — pointing, not doing.
pub fn first_run_hint(config_dir: &Path) -> Option<String> {
    if config_dir.exists() {
        return None;
    }
    Some(format!(
        "first run — no config at {}. `hotl setup` writes safe defaults; `hotl doctor` checks your setup.",
        config_dir.display()
    ))
}

/// Compare two `x.y.z` versions: is `latest` newer than `current`?
/// Non-numeric/short versions compare by the parts that parse (missing = 0).
pub fn is_newer(current: &str, latest: &str) -> bool {
    parts(latest) > parts(current)
}

fn parts(v: &str) -> (u64, u64, u64) {
    let v = v.trim_start_matches('v');
    let mut it = v.split(['.', '-', '+']).map(|p| p.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_writes_then_preserves() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("hotl");
        assert_eq!(setup_main(&cfg, false), 0);
        assert!(cfg.join("permissions.toml").exists());
        // Second run without --force keeps the (possibly edited) file.
        std::fs::write(cfg.join("permissions.toml"), "# edited\n").unwrap();
        setup_main(&cfg, false);
        assert_eq!(std::fs::read_to_string(cfg.join("permissions.toml")).unwrap(), "# edited\n");
        // --force overwrites.
        setup_main(&cfg, true);
        assert!(std::fs::read_to_string(cfg.join("permissions.toml")).unwrap().contains("allow"));
    }

    #[test]
    fn first_run_hint_only_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(first_run_hint(&dir.path().join("nope")).is_some());
        assert!(first_run_hint(dir.path()).is_none());
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.1.2", "0.2.0"));
        assert!(is_newer("0.1.2", "0.1.3"));
        assert!(is_newer("v0.1.2", "v1.0.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.2", "0.1.2"));
    }
}
