//! `hotl doctor` (MD): report what this machine's setup will actually do —
//! provider selection, sandbox floor, config, store health — before the user
//! burns a prompt finding out. Checks never mutate state.

use std::path::Path;

use hotl_platform::EnvSecrets;
use hotl_store::Masker;
use hotl_tools::sandbox;

enum Status {
    Ok,
    Warn,
    Fail,
}

struct Check {
    status: Status,
    line: String,
}

fn ok(line: String) -> Check {
    Check { status: Status::Ok, line }
}
fn warn(line: String) -> Check {
    Check { status: Status::Warn, line }
}
fn fail(line: String) -> Check {
    Check { status: Status::Fail, line }
}

pub fn doctor_main() -> i32 {
    let config_dir = crate::agent::config_dir();
    let sessions_dir = crate::agent::sessions_dir();
    let checks = [
        provider_check(),
        sandbox_check(),
        config_check(&config_dir),
        rules_check(&config_dir),
        sessions_check(&sessions_dir),
        memory_check(&config_dir),
        audit_check(&sessions_dir),
        undo_check(),
    ];
    println!("hotl {} — doctor", env!("CARGO_PKG_VERSION"));
    let mut failed = false;
    for check in &checks {
        let tag = match check.status {
            Status::Ok => "ok  ",
            Status::Warn => "warn",
            Status::Fail => {
                failed = true;
                "FAIL"
            }
        };
        println!("  {tag}  {}", check.line);
    }
    if failed {
        1
    } else {
        0
    }
}

fn provider_check() -> Check {
    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    match crate::agent::select_provider(&cfg, &EnvSecrets) {
        Ok((_, model, _source)) => ok(format!("provider: {model} selected (keys present)")),
        Err(msg) => fail(format!("provider: {}", msg.lines().next().unwrap_or(&msg))),
    }
}

fn sandbox_check() -> Check {
    match sandbox::probe() {
        sandbox::SandboxStatus::Enforced(m) => ok(format!("sandbox: enforced ({m})")),
        sandbox::SandboxStatus::Disabled => {
            warn("sandbox: disabled via HOTL_SANDBOX=off — every exec is individually gated".into())
        }
        sandbox::SandboxStatus::Unavailable(reason) => {
            warn(format!("sandbox: unavailable ({reason}) — every exec is individually gated"))
        }
    }
}

fn config_check(config_dir: &Path) -> Check {
    let cfg = config_dir.join("config.toml");
    if cfg.is_file() {
        ok(format!("config: {} loaded", cfg.display()))
    } else {
        ok(format!("config: none at {} (defaults; run `hotl setup`)", cfg.display()))
    }
}

fn rules_check(config_dir: &Path) -> Check {
    match crate::config::Config::load(config_dir).allow_toml() {
        None => ok("allow rules: none (every gated tool call asks)".into()),
        Some(t) => match hotl_tools::rules::Rules::from_toml(&t) {
            Ok(_) => ok("allow rules: [[allow]] in config.toml loaded".into()),
            Err(e) => warn(format!("allow rules: config.toml [[allow]] ignored: {e}")),
        },
    }
}

fn sessions_check(sessions_dir: &Path) -> Check {
    if let Err(e) = std::fs::create_dir_all(sessions_dir) {
        return fail(format!("sessions: cannot create {}: {e}", sessions_dir.display()));
    }
    let probe = sessions_dir.join(".doctor-probe");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            ok(format!("sessions: {} (writable)", sessions_dir.display()))
        }
        Err(e) => fail(format!("sessions: {} not writable: {e}", sessions_dir.display())),
    }
}

fn memory_check(config_dir: &Path) -> Check {
    match hotl_context::load_memory(config_dir) {
        Some(_) => ok("memory: memory/MEMORY.md loads at session start".into()),
        None => ok(format!(
            "memory: none (create {}/memory/MEMORY.md to enable)",
            config_dir.display()
        )),
    }
}

fn undo_check() -> Check {
    if hotl_store::shadow::git_available() {
        ok("undo: git found — sessions snapshot before/after mutating steps".into())
    } else {
        warn("undo: git not found — `hotl undo` snapshots are disabled".into())
    }
}

fn audit_check(sessions_dir: &Path) -> Check {
    let hits = hotl_store::audit_secrets(sessions_dir, &Masker::from_env());
    if hits.is_empty() {
        ok("secrets audit: no current secret values found in stored logs".into())
    } else {
        warn(format!(
            "secrets audit: {} log(s) contain values that are now secrets — rotate them (first: {})",
            hits.len(),
            hits[0].display()
        ))
    }
}
