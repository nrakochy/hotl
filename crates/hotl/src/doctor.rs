//! `hotl doctor` (MD): report what this machine's setup will actually do —
//! provider selection, sandbox floor, config, store health — before the user
//! burns a prompt finding out. Checks never mutate state.

use std::path::Path;

use hotl_platform::{EnvSecrets, SecretStore};
use hotl_store::Masker;
use hotl_tools::sandbox;

use crate::agent::AuthMode;

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

fn source_kind(auth: AuthMode, refreshable: bool) -> &'static str {
    match auth {
        AuthMode::Subscription => "subscription — no credential held",
        AuthMode::ApiKey if refreshable => "helper",
        AuthMode::ApiKey => "static env key",
    }
}

/// Where the capability probe goes. Shares `v1_base` with the providers so a
/// bare-origin base can't make doctor call a live endpoint unreachable.
fn models_url(base: &str) -> String {
    format!("{}/models", hotl_provider::v1_base(base))
}

fn probe_line(base: &str, result: Result<u16, String>, auth: AuthMode) -> String {
    let url = models_url(base);
    let subscription = auth == AuthMode::Subscription;
    match result {
        // Nothing was authenticated from hotl's side in subscription mode, so
        // don't claim a key was accepted.
        Ok(s) if s < 400 && subscription => format!("gateway: {url} reachable (HTTP {s})"),
        Ok(s) if s < 400 => format!("gateway: {url} reachable (HTTP {s}, key accepted)"),
        // "check the key source" is wrong guidance when hotl holds no key —
        // the endpoint failed to authenticate upstream, not hotl.
        Ok(s) if (s == 401 || s == 403) && subscription => format!(
            "gateway: {url} reachable but the endpoint is not authenticated (HTTP {s}) — \
             hotl holds no credential in subscription mode; re-authenticate the endpoint"
        ),
        Ok(s) if s == 401 || s == 403 => format!(
            "gateway: {url} reachable but rejected the key (HTTP {s}) — check the key source"
        ),
        Ok(s) => format!("gateway: {url} responded HTTP {s}"),
        Err(e) => format!("gateway: {url} unreachable ({e}) — is the gateway running?"),
    }
}

fn provider_check() -> Check {
    let cfg = crate::config::Config::load(&crate::agent::config_dir());
    let auth = crate::agent::auth_mode(&cfg, &EnvSecrets).unwrap_or(AuthMode::ApiKey);
    match crate::agent::select_provider(&cfg, &EnvSecrets) {
        Ok((_, model, source)) => ok(format!(
            "provider: {model} selected (auth: {})",
            source_kind(auth, source.refreshable())
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
    // Subscription mode never consults a key source, so running the helper
    // here would report a success that has no bearing on what hotl does.
    if crate::agent::auth_mode(&cfg, &EnvSecrets) == Ok(AuthMode::Subscription) {
        return ok("key helper: configured but unused (auth = \"subscription\")".into());
    }
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
    // Provider-aware: whichever provider is active, probe the endpoint it
    // will actually use. `None` means a direct connection to the vendor.
    let Some(base) = crate::agent::active_endpoint(&cfg, &EnvSecrets) else {
        return ok("gateway: no custom base_url (direct provider)".into());
    };
    let auth = crate::agent::auth_mode(&cfg, &EnvSecrets).unwrap_or(AuthMode::ApiKey);
    let key = if auth == AuthMode::Subscription {
        None
    } else {
        crate::agent::select_provider(&cfg, &EnvSecrets)
            .ok()
            .and_then(|(_, _, s)| rt.block_on(s.get()).ok().flatten())
    };
    let result = gateway_probe(rt, &base, key.as_deref());
    match &result {
        Ok(s) if *s == 401 || *s == 403 => warn(probe_line(&base, result, auth)),
        Ok(s) if *s < 500 => ok(probe_line(&base, result, auth)),
        _ => fail(probe_line(&base, result, auth)),
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
            .get(models_url(base))
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
    let admin =
        std::env::var("HOTL_PREAPPROVED").unwrap_or_else(|_| crate::agent::ADMIN_RULES_PATH.into());
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
        assert_eq!(source_kind(AuthMode::ApiKey, true), "helper");
        assert_eq!(source_kind(AuthMode::ApiKey, false), "static env key");
    }

    /// A user in subscription mode must not be told a credential is in play.
    #[test]
    fn subscription_source_line_says_no_credential_is_held() {
        let line = source_kind(AuthMode::Subscription, false);
        assert!(line.contains("subscription"), "{line}");
        assert!(!line.contains("key"), "{line}");
    }

    #[test]
    fn gateway_probe_line_formats() {
        assert_eq!(
            probe_line("http://localhost:8080/v1", Ok(200), AuthMode::ApiKey),
            "gateway: http://localhost:8080/v1/models reachable (HTTP 200, key accepted)"
        );
        assert_eq!(
            probe_line("http://localhost:8080/v1", Ok(401), AuthMode::ApiKey),
            "gateway: http://localhost:8080/v1/models reachable but rejected the key (HTTP 401) — check the key source"
        );
        assert!(probe_line(
            "http://x/v1",
            Err("connection refused".into()),
            AuthMode::ApiKey
        )
        .contains("connection refused"));
    }

    /// The probe must resolve a bare-origin base the same way the provider
    /// does, or doctor reports a working endpoint as unreachable.
    #[test]
    fn probe_normalizes_a_bare_origin_base() {
        assert_eq!(
            probe_line("http://127.0.0.1:3456", Ok(200), AuthMode::Subscription),
            "gateway: http://127.0.0.1:3456/v1/models reachable (HTTP 200)"
        );
    }

    /// "check the key source" is actively wrong guidance when hotl holds no
    /// key — the endpoint is what failed to authenticate, not hotl.
    #[test]
    fn subscription_probe_401_does_not_blame_the_key_source() {
        let line = probe_line("http://127.0.0.1:3456/v1", Ok(401), AuthMode::Subscription);
        assert!(!line.contains("key source"), "{line}");
        assert!(line.contains("endpoint"), "{line}");
    }

    #[test]
    fn subscription_probe_200_does_not_claim_a_key_was_accepted() {
        let line = probe_line("http://127.0.0.1:3456/v1", Ok(200), AuthMode::Subscription);
        assert!(!line.contains("key accepted"), "{line}");
        assert!(line.contains("reachable"), "{line}");
    }
}
