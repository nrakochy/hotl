//! `hotl doctor` (MD): report what this machine's setup will actually do —
//! provider selection, sandbox floor, config, store health — before the user
//! burns a prompt finding out. Checks never mutate state, with one deliberate
//! exception: missing `[sandbox].writable` dirs are created exactly as agent
//! startup would create them, so a creation failure is diagnosable here.

use std::path::Path;

use hotl_mcp::trust::TrustState;
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
    // Loaded once, here: every load re-runs the lint, and the containment floor
    // must be computed from the same rule set the sandbox section describes.
    let (rules, rule_warnings) =
        crate::agent::load_rules_reporting(&crate::config::Config::load(&config_dir));
    let mut checks = vec![provider_check()];
    checks.extend(sandbox_check(&config_dir, &rules));
    checks.extend(egress_checks(&config_dir));
    checks.extend([config_check(&config_dir), permissions_check(&config_dir)]);
    checks.extend(rules_check(&config_dir, rule_warnings));
    checks.push(sessions_check(&sessions_dir));
    checks.extend(mcp_check(&config_dir));
    checks.extend([
        memory_check(&config_dir),
        audit_check(&sessions_dir),
        undo_check(),
        key_helper,
        gateway,
    ]);
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

fn join_paths(paths: &[std::path::PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The read-carve as a traced section: every denied path on its own line, with
/// what put it there and whether anything can lift it. A containment set nobody
/// can trace back to a rule is one nobody will trust or maintain (plan 0025
/// task 4). Continuation lines are indented to sit under the status tag.
fn containment_section(
    deny: &hotl_tools::sandbox::ReadDeny,
    containment: &hotl_tools::rules::Containment,
) -> String {
    let mut out = String::from("containment · reads denied to shell commands");
    let mut row = |path: &std::path::Path, why: &str| {
        out.push_str(&format!("\n        {}    {why}", path.display()));
    };
    for p in &deny.always {
        row(p, "built-in, not liftable");
    }
    for p in &deny.secrets {
        row(p, "hotl default (lift: [sandbox].readable)");
    }
    for p in &deny.rules {
        // The rule text, or a fallback that still says where the path came from
        // rather than leaving a line nobody can account for.
        let why = containment
            .rule_for(p)
            .map(str::to_string)
            .unwrap_or_else(|| "a [[deny]] rule".to_string());
        row(p, &why);
    }
    if deny.secrets.is_empty() {
        out.push_str("\n        (every credential path lifted by [sandbox].readable)");
    }
    out.push_str(&unprojectable_block(containment));
    out
}

/// The deny rules with no kernel expression, as continuation lines. Not an
/// error: each is enforced in full in-process and only lacks kernel reach, so
/// the wording says what it still governs and what to write instead.
fn unprojectable_block(containment: &hotl_tools::rules::Containment) -> String {
    if containment.unprojectable.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "\n        not reaching shell commands ({}) — still enforced for hotl's own tools:",
        containment.unprojectable.len()
    );
    for why in &containment.unprojectable {
        out.push_str(&format!("\n          {why}"));
    }
    out
}

/// Resolves and *installs* the `[sandbox]` extras (set-once) before probing —
/// the same order agent startup uses — so the verdict certifies the widened
/// floor, and each refused/dubious entry gets its own warn line.
fn sandbox_check(config_dir: &Path, rules: &hotl_tools::rules::Rules) -> Vec<Check> {
    let cfg = crate::config::Config::load(config_dir);
    // `rules` came from the same `load_rules` agent startup uses, admin tier
    // included: `doctor` computing its own weaker rule set would have it
    // confidently describing a containment floor nobody has (plan 0025
    // watch-out 5).
    let (extras, warnings) = cfg
        .sandbox
        .resolve(config_dir, &crate::agent::data_dir(), rules);
    let mut checks: Vec<Check> = warnings
        .into_iter()
        .map(|w| warn(format!("sandbox: {w}")))
        .collect();
    let extra_note = if extras.writable.is_empty() {
        String::new()
    } else {
        format!(" · [sandbox].writable: {}", join_paths(&extras.writable))
    };
    // The resolved read-carve, printed so `hotl doctor` describes the floor
    // children actually get rather than the one config asked for (plan 0022),
    // and traced back to what put each path there (plan 0025).
    let containment = hotl_tools::rules::project(rules, &|p| dunce::canonicalize(p).ok());
    checks.push(if extras.read_deny.is_empty() {
        warn(format!(
            "sandbox: read-carve empty — sandboxed commands can read everything{}",
            unprojectable_block(&containment)
        ))
    } else {
        ok(containment_section(&extras.read_deny, &containment))
    });
    hotl_tools::sandbox::init_extras(extras);
    // Which shell runs bash commands, and how it was found — the "how" is what
    // tells a user what to install when it is missing.
    checks.push(match hotl_tools::shell::resolve_shell() {
        Ok(spec) => ok(format!(
            "shell: {} (found via {})",
            spec.path.display(),
            spec.how
        )),
        Err(why) => warn(format!(
            "shell: none — the `bash` tool is absent from the registry. {why}"
        )),
    });
    checks.push(match sandbox::probe() {
        sandbox::SandboxStatus::Enforced(m) => ok(format!("sandbox: enforced ({m}){extra_note}")),
        sandbox::SandboxStatus::Disabled => {
            warn("sandbox: disabled via HOTL_SANDBOX=off — every exec is individually gated".into())
        }
        sandbox::SandboxStatus::Unavailable(reason) => warn(format!(
            "sandbox: unavailable ({reason}) — every exec is individually gated"
        )),
    });
    // The probe tests the read carve in the same spawn; a host that cannot
    // enforce it keeps its `bash` allow-rules and says so instead (plan 0022
    // decision 5).
    match hotl_tools::sandbox::read_carve_enforced() {
        Some(true) => checks.push(ok(
            "sandbox: read carve enforced (verified by the probe)".into()
        )),
        Some(false) => checks.push(warn(
            "sandbox: the read carve is NOT enforced on this host — a sandboxed command \
             can read hotl's own config/state and your credentials; every ask is marked \
             `reads:open`"
                .into(),
        )),
        None => {}
    }
    for failure in hotl_tools::sandbox::read_carve_failures() {
        checks.push(warn(format!(
            "sandbox: the read carve could not open {failure} — reads under it are denied \
             to sandboxed commands, which is fail-closed but was not asked for"
        )));
    }
    checks
}

/// The egress posture, with the effective allowlist split by source. The list
/// hotl ships is the one an eventual default flip imposes on everyone, so it
/// has to be enumerable without reading source (0026 Step 1.3).
///
/// Reads the config rather than `egress_state()`: resolving the state starts
/// the filtering proxy, and `doctor` reports posture, it does not open sockets.
fn egress_checks(config_dir: &Path) -> Vec<Check> {
    let cfg = crate::config::Config::load(config_dir);
    let (policy, warning) = cfg.network.egress_policy();
    let mut checks: Vec<Check> = Vec::new();
    if let Some(warning) = warning {
        checks.push(fail(format!("egress: {warning}")));
    }
    match policy {
        hotl_tools::net::EgressPolicy::Open => {
            checks.push(ok("egress: open — unrestricted; the human gate is the \
                            exfiltration boundary"
                .into()));
        }
        hotl_tools::net::EgressPolicy::Off => {
            checks.push(ok(
                "egress: off — loopback and unix sockets only, no ask (no proxy in the path)"
                    .into(),
            ));
        }
        hotl_tools::net::EgressPolicy::Allowlist(effective) => {
            let starter = cfg.network.starter_count();
            let custom: Vec<&String> = effective.iter().skip(starter).collect();
            checks.push(ok(format!(
                "egress: allowlist — {} host{} total",
                effective.len(),
                if effective.len() == 1 { "" } else { "s" }
            )));
            if starter > 0 {
                checks.push(ok(format!(
                    "egress: {starter} from hotl's starter list \
                     (set [network].defaults = false to drop): {}",
                    hotl_tools::net::STARTER_ALLOW.join(", ")
                )));
            }
            checks.push(if custom.is_empty() {
                ok("egress: 0 from config.toml [network].allow".into())
            } else {
                ok(format!(
                    "egress: {} from config.toml [network].allow: {}",
                    custom.len(),
                    custom
                        .iter()
                        .map(|h| h.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            });
        }
    }
    checks
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
    let resolved = cfg.permissions.resolve(
        std::env::var("HOTL_PERMISSIONS").ok().as_deref(),
        std::env::var("HOTL_PLAN").ok().as_deref(),
    );
    if let Some(w) = resolved.warning {
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
    let mode_word = resolved.mode.as_str();
    let plan_note = if resolved.plan { " · plan" } else { "" };
    ok(format!(
        "permissions: {mode_word}{plan_note} (protected paths always ask){admin_note}"
    ))
}

/// The rule set, plus its lint. `Rules::lint` reports rules that can never
/// match and `lint_containment` adds the ones the kernel cannot reach; a lint
/// line nobody prints is a rule that fails silently, which is the shape of T1-7.
fn rules_check(config_dir: &Path, rule_warnings: Vec<String>) -> Vec<Check> {
    let cfg = crate::config::Config::load(config_dir);
    let mut checks = match cfg.rules_toml() {
        None => vec![ok("allow rules: none (every gated tool call asks)".into())],
        Some(t) => match hotl_tools::rules::Rules::from_toml(&t) {
            Ok(_) => vec![ok(
                "allow rules: [[allow]]/[[deny]] in config.toml loaded".into()
            )],
            Err(e) => vec![warn(format!(
                "allow rules: config.toml [[allow]] ignored: {e}"
            ))],
        },
    };
    // `rule_warnings` are the lines agent startup prints. The kernel-reach notes
    // among them are already listed under the containment section above, so only
    // the real problems — a rule that matches nothing, a refused admin file —
    // get a row here.
    for line in rule_warnings {
        if line.starts_with("note: ") {
            continue;
        }
        checks.push(warn(format!("rules: {line}")));
    }
    checks
}

/// MCP servers and what the trust gate will do with each. This is also where
/// `trust.toml`'s parse warnings surface: `build_registry` loads the store with
/// `TrustStore::load`, which drops them, so a corrupt file would otherwise show
/// up only as an unexplained wave of re-prompts.
fn mcp_check(config_dir: &Path) -> Vec<Check> {
    let servers = crate::config::Config::load(config_dir).mcp_servers();
    let (store, warnings) = hotl_mcp::trust::TrustStore::load_reporting(config_dir);
    let mut checks: Vec<Check> = warnings
        .into_iter()
        .map(|w| warn(format!("mcp: {w}")))
        .collect();
    // Stale grants are reported even with nothing configured — removing the
    // last `[[mcp]]` entry is precisely how a grant is orphaned.
    if servers.is_empty() {
        checks.push(ok("mcp: no servers configured".into()));
        checks.extend(stale_grant_checks(&store, &servers));
        return checks;
    }
    let workspace = hotl_tools::workspace_root();
    let states: Vec<_> = servers
        .iter()
        .map(|s| {
            let fp = hotl_mcp::trust::Fingerprint::of(s);
            (s.name.as_str(), store.state(&s.name, &fp, workspace))
        })
        .collect();
    let count = |want| states.iter().filter(|(_, s)| *s == want).count();
    checks.push(ok(format!(
        "mcp: {} server(s) configured ({} trusted, {} screen on first use)",
        servers.len(),
        count(TrustState::Trusted),
        count(TrustState::Untrusted)
    )));
    for (name, _) in states.iter().filter(|(_, s)| *s == TrustState::Unhashable) {
        checks.push(warn(format!(
            "mcp: `{name}`'s binary cannot be read, so hotl will ask every time — \
             fix the path, or run: hotl mcp untrust {name}"
        )));
    }
    checks.extend(stale_grant_checks(&store, &servers));
    checks
}

/// Grants whose server has left the config. Harmless — the fingerprint must
/// still match — but they are the residue nothing else reports.
fn stale_grant_checks(
    store: &hotl_mcp::trust::TrustStore,
    servers: &[hotl_mcp::config::ServerConfig],
) -> Vec<Check> {
    let mut stale: Vec<_> = store
        .entries()
        .map(|(n, _)| n.to_string())
        .filter(|n| !servers.iter().any(|s| &s.name == n))
        .collect();
    stale.sort();
    stale
        .into_iter()
        .map(|n| {
            warn(format!(
                "mcp: trust.toml holds a grant for `{n}`, which is no longer \
                 configured — run: hotl mcp untrust {n}"
            ))
        })
        .collect()
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

/// Undo-point status from on-disk state (0035 decision 11): doctor is a
/// separate process, so it reports what the newest session's shadow shows —
/// the in-process worker's live view is the TUI strip's job.
fn undo_check() -> Check {
    if !hotl_store::shadow::git_available() {
        return warn("undo: git not found — `hotl undo` snapshots are disabled".into());
    }
    let root = crate::agent::shadow_root();
    let opened = hotl_store::shadow::latest_session(&root)
        .and_then(|s| hotl_store::shadow::Shadow::open(&root, &s).map(|sh| (s, sh)));
    let Some((session, shadow)) = opened else {
        return ok("undo: git found — sessions snapshot at quiet windows".into());
    };
    if !shadow.has_mutations() {
        return ok(format!("undo: no agent mutations in session {session}"));
    }
    match shadow.latest_clean() {
        Some((_, label)) => ok(format!("undo: ready (\"{label}\", session {session})")),
        None => warn(format!(
            "undo: warming — no snapshot yet in session {session}"
        )),
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

    fn lines(checks: &[Check]) -> String {
        checks
            .iter()
            .map(|c| {
                let tag = match c.status {
                    Status::Ok => "ok",
                    Status::Warn => "warn",
                    Status::Fail => "FAIL",
                };
                format!("{tag} {}\n", c.line)
            })
            .collect()
    }

    #[test]
    fn mcp_check_counts_states_and_flags_unreadable_binaries() {
        let dir = tempfile::tempdir().unwrap();
        assert!(lines(&mcp_check(dir.path())).contains("ok mcp: no servers configured"));

        let bin = dir.path().join("server-bin");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "[[mcp]]\nname = \"docs\"\ncommand = {}\n\n\
                 [[mcp]]\nname = \"broken\"\ncommand = \"/definitely/not/here\"\n",
                crate::config::toml_path(&bin)
            ),
        )
        .unwrap();
        let out = lines(&mcp_check(dir.path()));
        assert!(out.contains("2 server(s) configured"), "{out}");
        assert!(out.contains("0 trusted, 1 screen on first use"), "{out}");
        assert!(
            out.contains("warn mcp: `broken`'s binary cannot be read"),
            "{out}"
        );
    }

    /// `build_registry` loads the trust store with `load`, which drops parse
    /// warnings. Doctor is where they surface.
    #[test]
    fn mcp_check_surfaces_a_corrupt_trust_file_and_stale_grants() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trust.toml"), b"not = valid toml [").unwrap();
        let out = lines(&mcp_check(dir.path()));
        assert!(
            out.contains("warn mcp:") && out.contains("trust.toml"),
            "{out}"
        );

        std::fs::write(dir.path().join("trust.toml"), b"ghost = \"fp2:abc\"\n").unwrap();
        let out = lines(&mcp_check(dir.path()));
        assert!(out.contains("hotl mcp untrust ghost"), "{out}");
    }

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
