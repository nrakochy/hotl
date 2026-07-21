//! `hotl doctor` (MD): report what this machine's setup will actually do —
//! provider selection, sandbox floor, config, store health — before the user
//! burns a prompt finding out. Checks never mutate state.

use std::path::Path;

use hotl_platform::{EnvSecrets, SecretStore};
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
    Check {
        status: Status::Ok,
        line,
    }
}
fn warn(line: String) -> Check {
    Check {
        status: Status::Warn,
        line,
    }
}
fn fail(line: String) -> Check {
    Check {
        status: Status::Fail,
        line,
    }
}

pub fn doctor_main() -> i32 {
    let config_dir = crate::agent::config_dir();
    let sessions_dir = crate::agent::sessions_dir();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build doctor's single-threaded runtime");
    let key_helper = key_helper_check(&rt);
    let gateway = gateway_check(&rt);
    let checks = [
        provider_check(),
        sandbox_check(),
        config_check(&config_dir),
        permissions_check(&config_dir),
        rules_check(&config_dir),
        sessions_check(&sessions_dir),
        memory_check(&config_dir),
        audit_check(&sessions_dir),
        undo_check(),
        key_helper,
        gateway,
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

fn source_kind(refreshable: bool) -> &'static str {
    if refreshable {
        "helper"
    } else {
        "static env key"
    }
}

fn probe_line(base: &str, result: Result<u16, String>) -> String {
    let url = format!("{}/models", base.trim_end_matches('/'));
    match result {
        Ok(s) if s < 400 => format!("gateway: {url} reachable (HTTP {s}, key accepted)"),
        Ok(s) if s == 401 || s == 403 => format!(
            "gateway: {url} reachable but rejected the key (HTTP {s}) — check the key source"
        ),
        Ok(s) => format!("gateway: {url} responded HTTP {s}"),
        Err(e) => format!("gateway: {url} unreachable ({e}) — is the gateway running?"),
    }
}

fn provider_check() -> Check {
    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    match crate::agent::select_provider(&cfg, &EnvSecrets) {
        Ok((_, model, source)) => ok(format!(
            "provider: {model} selected (key source: {})",
            source_kind(source.refreshable())
        )),
        Err(msg) => fail(format!("provider: {}", msg.lines().next().unwrap_or(&msg))),
    }
}

fn key_helper_check(rt: &tokio::runtime::Runtime) -> Check {
    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    let helper = EnvSecrets
        .get("HOTL_API_KEY_HELPER")
        .or_else(|| cfg.provider.api_key_helper.clone());
    let Some(_) = helper else {
        return ok("key helper: not configured (static env keys)".into());
    };
    let (_, _, source) = match crate::agent::select_provider(&cfg, &EnvSecrets) {
        Ok(t) => t,
        Err(_) => {
            return warn("key helper: configured, but provider selection failed (see above)".into())
        }
    };
    let start = std::time::Instant::now();
    match rt.block_on(source.get()) {
        Ok(Some(_)) => ok(format!(
            "key helper: OK ({:.1}s, key masked)",
            start.elapsed().as_secs_f32()
        )),
        Ok(None) => warn("key helper: ran but produced no key".into()),
        Err(e) => fail(format!("key helper: {e}")),
    }
}

fn gateway_check(rt: &tokio::runtime::Runtime) -> Check {
    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    let base = match EnvSecrets
        .get("HOTL_OPENAI_BASE_URL")
        .or_else(|| cfg.provider.base_url.clone())
    {
        Some(b) if b != hotl_provider_openai::DEFAULT_BASE_URL => b,
        _ => return ok("gateway: no custom base_url (direct provider)".into()),
    };
    let key = crate::agent::select_provider(&cfg, &EnvSecrets)
        .ok()
        .and_then(|(_, _, s)| rt.block_on(s.get()).ok().flatten());
    let result = gateway_probe(rt, &base, key.as_deref());
    match &result {
        Ok(s) if *s == 401 || *s == 403 => warn(probe_line(&base, result)),
        Ok(s) if *s < 500 => ok(probe_line(&base, result)),
        _ => fail(probe_line(&base, result)),
    }
}

fn gateway_probe(
    rt: &tokio::runtime::Runtime,
    base: &str,
    key: Option<&str>,
) -> Result<u16, String> {
    rt.block_on(async {
        let client = reqwest::Client::new();
        let mut req = client
            .get(format!("{}/models", base.trim_end_matches('/')))
            .timeout(std::time::Duration::from_secs(5));
        if let Some(k) = key {
            req = req.bearer_auth(k);
        }
        req.send()
            .await
            .map(|r| r.status().as_u16())
            .map_err(|e| e.to_string())
    })
}

fn sandbox_check() -> Check {
    match sandbox::probe() {
        sandbox::SandboxStatus::Enforced(m) => ok(format!("sandbox: enforced ({m})")),
        sandbox::SandboxStatus::Disabled => {
            warn("sandbox: disabled via HOTL_SANDBOX=off — every exec is individually gated".into())
        }
        sandbox::SandboxStatus::Unavailable(reason) => warn(format!(
            "sandbox: unavailable ({reason}) — every exec is individually gated"
        )),
    }
}

fn config_check(config_dir: &Path) -> Check {
    let cfg = config_dir.join("config.toml");
    if cfg.is_file() {
        ok(format!("config: {} loaded", cfg.display()))
    } else {
        ok(format!(
            "config: none at {} (defaults; run `hotl setup`)",
            cfg.display()
        ))
    }
}

fn permissions_check(config_dir: &Path) -> Check {
    let cfg = crate::config::Config::load(config_dir);
    let (mode, warning) = cfg
        .permissions
        .resolve(std::env::var("HOTL_PERMISSIONS").ok().as_deref());
    if let Some(w) = warning {
        return warn(format!("permissions: {w}"));
    }
    if hotl_tools::rules::enforced_build() {
        return ok("permissions: enforced (build) — mode config is ignored".into());
    }
    let admin = std::env::var("HOTL_PREAPPROVED")
        .unwrap_or_else(|_| crate::agent::ADMIN_RULES_PATH.into());
    let admin_note = match crate::agent::load_admin(Path::new(&admin)) {
        Ok(Some(_)) => format!(" · admin rules: {admin}"),
        Ok(None) => String::new(),
        Err(why) => return warn(format!("permissions: admin file {admin} refused — {why}")),
    };
    let mode_word = match mode {
        hotl_tools::rules::PermissionMode::Auto => "auto",
        hotl_tools::rules::PermissionMode::Ask => "ask",
    };
    ok(format!(
        "permissions: {mode_word} (protected paths always ask){admin_note}"
    ))
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
        return fail(format!(
            "sessions: cannot create {}: {e}",
            sessions_dir.display()
        ));
    }
    let probe = sessions_dir.join(".doctor-probe");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            ok(format!("sessions: {} (writable)", sessions_dir.display()))
        }
        Err(e) => fail(format!(
            "sessions: {} not writable: {e}",
            sessions_dir.display()
        )),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_source_line_names_active_source() {
        assert_eq!(source_kind(true), "helper");
        assert_eq!(source_kind(false), "static env key");
    }

    #[test]
    fn gateway_probe_line_formats() {
        assert_eq!(
            probe_line("http://localhost:8080/v1", Ok(200)),
            "gateway: http://localhost:8080/v1/models reachable (HTTP 200, key accepted)"
        );
        assert_eq!(
            probe_line("http://localhost:8080/v1", Ok(401)),
            "gateway: http://localhost:8080/v1/models reachable but rejected the key (HTTP 401) — check the key source"
        );
        assert!(probe_line("http://x/v1", Err("connection refused".into()))
            .contains("connection refused"));
    }
}
