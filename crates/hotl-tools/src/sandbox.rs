//! The kernel sandbox floor for `bash` (SECURITY.md layer 3).
//!
//! Write-confinement: the command (and its whole process tree) can read
//! everywhere but write only under the working directory, the temp dir, and
//! /dev. Network egress is open by default (the agent legitimately curls);
//! `[network].egress` in config.toml opts into confinement (see `net.rs`),
//! which this module enforces at the kernel when asked to.
//!
//! - macOS: Seatbelt via `sandbox-exec` (deprecated by Apple, still the
//!   mechanism its own tooling uses; profile passed inline with parameters).
//! - Linux: Landlock (kernel ≥ 5.13), applied in `pre_exec` after fork.
//! - Anywhere else, or kernels without Landlock: **fail-closed degradation**
//!   to the M0 posture — the command still runs only behind the y/n gate, and
//!   the ask is loudly marked UNSANDBOXED.
//!
//! `HOTL_SANDBOX=off` is the documented escape hatch (marked in the ask).

use std::path::PathBuf;

use crate::net::EgressState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Confinement active; the str names the mechanism.
    Enforced(&'static str),
    /// No floor on this host; reason attached. The y/n gate is the only guard.
    Unavailable(String),
    /// Explicitly disabled via HOTL_SANDBOX=off.
    Disabled,
}

/// Whether the container/orchestrator daemon socket class is reachable from a
/// confined command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSockets {
    /// Deny the container/orchestrator daemon socket class (default).
    DenyDaemons,
    /// Everything allowed — `HOTL_UNIX_SOCKETS=open`. Labeled in every ask.
    Open,
}

/// Whether a confined command may drive other applications through Apple
/// Events / LaunchServices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Automation {
    Deny,
    Allow,
}

fn unix_socket_policy() -> UnixSockets {
    if std::env::var("HOTL_UNIX_SOCKETS").is_ok_and(|v| v == "open") {
        UnixSockets::Open
    } else {
        UnixSockets::DenyDaemons
    }
}

fn automation_policy() -> Automation {
    if std::env::var("HOTL_MACOS_AUTOMATION").is_ok_and(|v| v == "allow") {
        Automation::Allow
    } else {
        Automation::Deny
    }
}

impl SandboxStatus {
    pub fn label(&self) -> String {
        match self {
            SandboxStatus::Enforced(m) => label_with(m, unix_socket_policy(), automation_policy()),
            SandboxStatus::Unavailable(_) => "UNSANDBOXED".to_string(),
            SandboxStatus::Disabled => "UNSANDBOXED(by HOTL_SANDBOX=off)".to_string(),
        }
    }
}

/// Pure renderer. Silence is the hardened default; a marker appears only
/// where the operator opted *out* of a denial — the same convention
/// `net::label_suffix` uses (no marker under the default Open egress).
fn label_with(mechanism: &str, unix: UnixSockets, automation: Automation) -> String {
    let mut out = format!("sandboxed:{mechanism}");
    if matches!(unix, UnixSockets::Open) {
        out.push_str(" unix:open");
    }
    if matches!(automation, Automation::Allow) {
        out.push_str(" automation:allow");
    }
    out
}

fn canon(p: PathBuf) -> PathBuf {
    p.canonicalize().unwrap_or(p)
}

/// Hard ceiling on the smoke test. A wedged `sandbox-exec` must never wedge
/// startup; on expiry the child is killed and the verdict is Unavailable
/// (fail-closed).
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Probe what this host can enforce.
///
/// INVARIANT: `Enforced(m)` is returned only after a child spawned under `m`
/// *failed* to create a file outside the confinement on this host — the
/// mechanism's presence on disk is never sufficient (D-9). Enforced by
/// `tests/sandbox_probe.rs::probe_refuses_a_mechanism_that_does_not_confine`.
///
/// Memoized: exactly one smoke-test spawn per process. `builtins.rs`,
/// `doctor.rs`, `shell_hooks.rs` and `agent.rs` all call this directly.
pub fn probe() -> SandboxStatus {
    static STATUS: std::sync::OnceLock<SandboxStatus> = std::sync::OnceLock::new();
    STATUS.get_or_init(probe_uncached).clone()
}

fn probe_uncached() -> SandboxStatus {
    if std::env::var("HOTL_SANDBOX").is_ok_and(|v| v == "off") {
        return SandboxStatus::Disabled;
    }
    let mechanism = match mechanism_available() {
        Ok(m) => m,
        Err(reason) => return SandboxStatus::Unavailable(reason),
    };
    match verify_confinement_with(mechanism, |script| {
        build_command(
            script,
            &SandboxStatus::Enforced(mechanism),
            &EgressState::Open,
        )
    }) {
        Ok(()) => SandboxStatus::Enforced(mechanism),
        Err(reason) => SandboxStatus::Unavailable(reason),
    }
}

/// Is the mechanism *present*? (The old `probe` stopped here — that is D-9.)
fn mechanism_available() -> Result<&'static str, String> {
    #[cfg(target_os = "macos")]
    {
        if std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            return Ok("seatbelt");
        }
        return Err("sandbox-exec not found".into());
    }
    #[cfg(target_os = "linux")]
    {
        return linux_mechanism();
    }
    #[allow(unreachable_code)]
    Err("no sandbox mechanism for this OS".into())
}

/// The highest Landlock ABI this kernel actually honors. `HardRequirement`
/// makes an unsupported level an error rather than a silent downgrade, so the
/// first level that builds is the truth. `None` = no Landlock at all.
///
/// (`landlock`'s own current-ABI query is private in 0.4.5 — the ladder is the
/// supported public route. The list is bounded by the ABI levels that crate
/// version knows about; a newer kernel simply reports the highest it names.)
#[cfg(target_os = "linux")]
fn landlock_abi() -> Option<landlock::ABI> {
    use landlock::{Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, ABI};
    static ABI_LEVEL: std::sync::OnceLock<Option<ABI>> = std::sync::OnceLock::new();
    *ABI_LEVEL.get_or_init(|| {
        for abi in [
            ABI::V7,
            ABI::V6,
            ABI::V5,
            ABI::V4,
            ABI::V3,
            ABI::V2,
            ABI::V1,
        ] {
            let ok = Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessFs::from_all(abi))
                .and_then(|r| r.create());
            if ok.is_ok() {
                return Some(abi);
            }
        }
        None
    })
}

/// Which Landlock posture this kernel earns. ABI v3 (Linux 6.2) is the first
/// level carrying the truncate right; below it the floor is genuinely partial
/// and is never certified silently.
#[cfg(target_os = "linux")]
fn linux_mechanism() -> Result<&'static str, String> {
    let Some(abi) = landlock_abi() else {
        return Err("landlock unavailable: kernel has no Landlock support".into());
    };
    if (abi as i32) >= 3 {
        return Ok("landlock");
    }
    if std::env::var("HOTL_SANDBOX").is_ok_and(|v| v == "best-effort") {
        return Ok("landlock(partial)");
    }
    Err(format!(
        "landlock ABI v{} (kernel < 6.2): the truncate right does not exist here, so a \
         `truncate(2)` by path outside the workspace is unconfined. Set \
         HOTL_SANDBOX=best-effort to accept the partial floor (every ask is labeled \
         `sandboxed:landlock(partial)`), or upgrade the kernel.",
        abi as i32
    ))
}

/// A directory we can really write to that is **outside** everything the
/// floor re-allows (cwd, `TMPDIR`, `/private/tmp`, `/dev`). Returning it is a
/// positive control: the parent creates and deletes a file here first, so a
/// later "the file does not exist" assertion cannot pass merely because the
/// path was unwritable anyway (the vacuous-negative trap).
pub fn probe_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("HOTL_SANDBOX_PROBE_DIR") {
        return writable(&canon(PathBuf::from(dir)));
    }
    let allowed: Vec<PathBuf> = vec![
        canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        canon(std::env::temp_dir()),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/dev"),
    ];
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("/var/tmp")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home));
    }
    let mut last = "no candidate directory outside the confinement".to_string();
    for cand in candidates {
        let cand = canon(cand);
        if allowed.iter().any(|a| cand.starts_with(a)) {
            continue; // inside the write set — proves nothing
        }
        match writable(&cand) {
            Ok(dir) => return Ok(dir),
            Err(e) => last = e,
        }
    }
    Err(format!(
        "cannot verify the sandbox: {last}. Set HOTL_SANDBOX_PROBE_DIR to a writable \
         directory outside the working directory and outside TMPDIR."
    ))
}

fn writable(dir: &std::path::Path) -> Result<PathBuf, String> {
    let probe = dir.join(format!("hotl-sbx-writable-{}", std::process::id()));
    std::fs::write(&probe, b"x").map_err(|e| format!("{} is not writable: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(dir.to_path_buf())
}

/// Single-quote a path for `sh -c`. `None` when the path cannot be quoted
/// safely (a NUL or a non-UTF-8 component) — the caller then falls through to
/// a fail-closed verdict rather than building a dubious shell word.
fn shell_single_quote(p: &std::path::Path) -> Option<String> {
    let s = p.to_str()?;
    if s.contains('\0') {
        return None;
    }
    Some(format!("'{}'", s.replace('\'', r"'\''")))
}

/// Spawn a child under `build` that tries to create a file outside the
/// confinement, and certify the mechanism only if it fails **and** the file
/// does not exist afterwards. `build` is injected so the negative case is
/// testable on any host, with no sandbox required.
pub fn verify_confinement_with(
    mechanism: &str,
    build: impl Fn(&str) -> tokio::process::Command,
) -> Result<(), String> {
    let dir = probe_dir()?;
    let target = dir.join(format!(
        "hotl-sbx-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&target);
    let quoted = shell_single_quote(&target)
        .ok_or_else(|| format!("cannot verify {mechanism}: unquotable probe path"))?;
    let outcome = run_bounded(build(&format!("echo hotl-probe > {quoted}")));
    let leaked = target.exists();
    if leaked {
        let _ = std::fs::remove_file(&target);
    }
    match outcome {
        Ok(success) if leaked || success => Err(format!(
            "`{mechanism}` did not confine a write to {} — the profile/ruleset is not \
             being applied on this host, so bash cannot be auto-approved",
            target.display()
        )),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("`{mechanism}` could not be verified: {e}")),
    }
}

/// Run to completion under `PROBE_TIMEOUT` using the **std** command inside
/// the tokio one: `probe()` is sync and is called from contexts with and
/// without a tokio runtime (`doctor.rs` vs `builtins.rs`), so it must not
/// depend on a reactor. `as_std_mut` keeps any `pre_exec` closure the Landlock
/// path installed. Returns Ok(child_succeeded).
fn run_bounded(mut cmd: tokio::process::Command) -> std::io::Result<bool> {
    let std_cmd = cmd.as_std_mut();
    std_cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = std_cmd.spawn()?;
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status.success()),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("probe did not exit within {PROBE_TIMEOUT:?}"),
                ));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
}

/// Build the command for `sh -c <command>` under the active floor and the
/// resolved egress state. With `EgressState::Open` the result is byte-identical
/// to the pre-egress behavior.
pub fn build_command(
    command: &str,
    status: &SandboxStatus,
    egress: &EgressState,
) -> tokio::process::Command {
    let cmd = match status {
        SandboxStatus::Enforced("seatbelt") => seatbelt_command(command, egress),
        #[cfg(target_os = "linux")]
        SandboxStatus::Enforced("landlock") | SandboxStatus::Enforced("landlock(partial)") => {
            landlock_command(command, egress)
        }
        _ => {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    };
    apply_child_env(cmd, egress)
}

/// Build a direct `program arg...` invocation (no shell) under the active
/// floor and the resolved egress state — the argv-safe sibling of
/// `build_command`, for callers (like `grep`) that must never splice a
/// model-authored value into a shell string. Same sandbox floor, same proxy
/// env wiring; only the exec shape differs (`program`+`args` vs `sh -c`).
pub fn build_argv(
    program: &str,
    args: &[String],
    status: &SandboxStatus,
    egress: &EgressState,
) -> tokio::process::Command {
    let cmd = match status {
        SandboxStatus::Enforced("seatbelt") => seatbelt_argv(program, args, egress),
        #[cfg(target_os = "linux")]
        SandboxStatus::Enforced("landlock") | SandboxStatus::Enforced("landlock(partial)") => {
            landlock_argv(program, args, egress)
        }
        _ => {
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args);
            cmd
        }
    };
    apply_child_env(cmd, egress)
}

/// Credentials the *harness* holds. A child process — a model-authored bash
/// command, `rg`, a diagnostic, an owner hook — has no legitimate need for
/// any of them, and an auto-approved `bash` can otherwise read them with
/// `env`. Deliberately narrow: the Masker's KEY/TOKEN/SECRET heuristic
/// (hotl-store/src/lib.rs) would also strip GITHUB_TOKEN /
/// CARGO_REGISTRY_TOKEN / NPM_TOKEN and silently break `gh`, `cargo publish`
/// and `npm publish`. `HOTL_SCRUB_ENV_STRICT=1` opts into that heuristic.
const PROVIDER_CREDENTIAL_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_ADMIN_KEY",
    "OPENAI_API_KEY",
    "HOTL_API_KEY_HELPER",
    "HOTL_SCRUB_ENV",
    "HOTL_SCRUB_ENV_STRICT",
];

/// Same markers as `hotl_store::Masker` — duplicated rather than depended on
/// (hotl-tools does not link hotl-store). R7 owns consolidating the sanitizer
/// copies; this list joins that queue.
const SECRET_NAME_MARKERS: [&str; 7] = [
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
];

fn names_to_scrub(extra: Option<&str>, strict: bool) -> Vec<String> {
    let mut names: Vec<String> = PROVIDER_CREDENTIAL_VARS
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(extra) = extra {
        names.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
        );
    }
    if strict {
        for (name, value) in std::env::vars() {
            let upper = name.to_uppercase();
            if value.len() >= 8 && SECRET_NAME_MARKERS.iter().any(|m| upper.contains(m)) {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The one place a child's environment is shaped. Both `build_command` and
/// `build_argv` end here, so there is no second spawn path to forget.
///
/// INVARIANT: no provider credential is present in any child's environment.
/// Enforced by `provider_credentials_never_reach_the_child`.
fn apply_child_env(
    mut cmd: tokio::process::Command,
    egress: &EgressState,
) -> tokio::process::Command {
    for name in names_to_scrub(
        std::env::var("HOTL_SCRUB_ENV").ok().as_deref(),
        std::env::var("HOTL_SCRUB_ENV_STRICT").is_ok_and(|v| v == "1"),
    ) {
        cmd.env_remove(name);
    }
    apply_proxy_env(cmd, egress)
}

/// Allowlist mode: cooperating clients (curl, git, pip, cargo — anything
/// honoring the proxy env) route through the filtering proxy on loopback;
/// non-cooperating clients hit the kernel loopback-only wall and fail closed.
/// For Off/Open, no env — the kernel does all the work (or nothing changes).
fn apply_proxy_env(
    mut cmd: tokio::process::Command,
    egress: &EgressState,
) -> tokio::process::Command {
    if let EgressState::Proxy(port) = egress {
        // The credential rides the standard proxy URL, which curl, git, pip
        // and cargo all forward as `Proxy-Authorization`.
        let proxy = match crate::net::proxy_user_info() {
            Some(userinfo) => format!("http://{userinfo}@127.0.0.1:{port}"),
            None => format!("http://127.0.0.1:{port}"),
        };
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
        ] {
            cmd.env(key, &proxy);
        }
        for key in ["NO_PROXY", "no_proxy"] {
            cmd.env(key, "localhost,127.0.0.1,::1");
        }
    }
    cmd
}

/// The Seatbelt profile, pure (unit-tested against drift). Write-deny by
/// default with the working tree, temp, and /dev re-allowed; when
/// `confine_network`, deny all network and re-allow unix-domain sockets and
/// loopback (the mDNSResponder unix socket means DNS resolution still works —
/// documented in SECURITY.md as a resolution, not exfil-confinement, limit).
#[cfg(target_os = "macos")]
fn seatbelt_profile(confine_network: bool, unix_sockets: UnixSockets) -> String {
    let mut profile = String::from(
        r#"(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (subpath (param "CWD"))
  (subpath (param "TMP"))
  (subpath "/private/tmp")
  (subpath "/dev"))
"#,
    );
    if confine_network {
        profile.push_str(
            r#"(deny network*)
(allow network* (local unix) (remote unix))
(allow network-outbound (remote ip "localhost:*"))
(allow network-inbound (local ip "localhost:*"))
"#,
        );
    }
    // Kept at the *end* of the profile so no later clause can widen it back. A
    // unix socket connect is a network operation, not a file write, so the
    // file-write floor above does not touch it — and `/var/run/docker.sock` is
    // a root-equivalent escape (mount the host root, write anywhere).
    // Not denied here: ssh-agent/gpg-agent (capability-limited, and denying
    // them breaks `git push` over SSH) — see docs/SECURITY.md.
    // INVARIANT: the daemon socket class is unreachable unless the operator
    // set HOTL_UNIX_SOCKETS=open. Enforced by
    // `seatbelt_blocks_a_connect_to_a_denied_daemon_socket`.
    if matches!(unix_sockets, UnixSockets::DenyDaemons) {
        profile.push_str(
            "(deny network-outbound (regex #\"(docker|podman|containerd|crio|dockershim)[^/]*\\.sock$\"))\n\
             (deny network-outbound (regex #\"^/(private/)?(var/)?run/(docker|containerd|crio)/\"))\n",
        );
    }
    profile
}

/// The `sandbox-exec` invocation up to (not including) the program to run —
/// shared by the `sh -c` and direct-argv exec shapes.
#[cfg(target_os = "macos")]
fn seatbelt_base(egress: &EgressState) -> tokio::process::Command {
    let confine_network = matches!(egress, EgressState::Off | EgressState::Proxy(_));
    let cwd = canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let tmp = canon(std::env::temp_dir());
    let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-p")
        .arg(seatbelt_profile(confine_network, unix_socket_policy()))
        .arg("-D")
        .arg(format!("CWD={}", cwd.display()))
        .arg("-D")
        .arg(format!("TMP={}", tmp.display()));
    cmd
}

#[cfg(target_os = "macos")]
fn seatbelt_command(command: &str, egress: &EgressState) -> tokio::process::Command {
    let mut cmd = seatbelt_base(egress);
    cmd.arg("sh").arg("-c").arg(command);
    cmd
}

#[cfg(target_os = "macos")]
fn seatbelt_argv(program: &str, args: &[String], egress: &EgressState) -> tokio::process::Command {
    let mut cmd = seatbelt_base(egress);
    cmd.arg(program).args(args);
    cmd
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn seatbelt_command(command: &str, _egress: &EgressState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn seatbelt_argv(program: &str, args: &[String], _egress: &EgressState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd
}

/// Can this kernel enforce Landlock **net** rules (ABI v4, kernel ≥ 6.7)?
/// Handled as a `HardRequirement` so a pre-6.7 kernel errors out here instead
/// of silently skipping net enforcement — the caller degrades to the loud
/// `Unenforced` posture, never to open-and-quiet.
#[cfg(target_os = "linux")]
pub(crate) fn landlock_net_supported() -> Result<(), String> {
    use std::sync::OnceLock;

    use landlock::{AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr};

    static SUPPORT: OnceLock<Result<(), String>> = OnceLock::new();
    SUPPORT
        .get_or_init(|| {
            match Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessNet::ConnectTcp)
            {
                Ok(r) => match r.create() {
                    Ok(_) => Ok(()),
                    Err(e) => Err(format!("landlock net unavailable: {e}")),
                },
                Err(e) => Err(format!("landlock net needs kernel ≥ 6.7: {e}")),
            }
        })
        .clone()
}

/// Build the fully-populated ruleset **in the parent**: `pre_exec` runs
/// between fork and exec in a multithreaded process, where allocation
/// (malloc lock) and other non-async-signal-safe work can deadlock, so
/// everything that allocates happens here, before the spawn.
///
/// The fs part stays best-effort at ABI::V2 as before. The net part
/// (Off/Proxy egress) is a **hard requirement**: `ConnectTcp` only — zero
/// allowed ports for Off, exactly the proxy port for Proxy. Two honest Linux
/// limits, by Landlock's design: net rules are **TCP-only** (UDP — including
/// DNS and DNS-tunnel exfiltration — is not confined) and **port-scoped, not
/// address-scoped** (the proxy port number is connectable on any host, and
/// Off blocks loopback TCP too; unix-domain sockets are untouched either
/// way). Shared by `landlock_command` and `landlock_argv`.
#[cfg(target_os = "linux")]
fn build_landlock_ruleset(egress: &EgressState) -> Option<std::os::unix::io::OwnedFd> {
    use landlock::{
        Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr, ABI,
    };

    // ABI v3 (Linux 6.2) is the first level that carries
    // LANDLOCK_ACCESS_FS_TRUNCATE. Landlock only restricts rights present in
    // the *handled* mask, so pinning v2 here left `truncate(2)` by path
    // unconfined on every kernel, not merely on old ones — a raw
    // `truncate(2)` (not `truncate -s 0`, which opens O_WRONLY first and is
    // already denied by WriteFile) could zero any file on the host from an
    // auto-approved bash. BestEffort still degrades gracefully below v3 —
    // `probe()` is what reports the degradation instead of hiding it.
    // INVARIANT: the handled mask includes Truncate wherever the kernel offers
    // it. Enforced by `landlock_confines_truncate_by_path`.
    let abi = ABI::V3;
    let cwd = canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let tmp = canon(std::env::temp_dir());
    let confine_network = matches!(egress, EgressState::Off | EgressState::Proxy(_));
    let mut attr = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .ok()?;
    if confine_network {
        // HardRequirement: on a kernel without the net ABI this fails,
        // build_landlock_ruleset returns None, and the child refuses to exec
        // (fail-closed) rather than run with open egress.
        attr = attr
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessNet::ConnectTcp)
            .ok()?
            .set_compatibility(CompatLevel::BestEffort);
    }
    let mut ruleset = attr.create().ok()?;
    // Read + execute everywhere.
    ruleset = ruleset
        .add_rule(landlock::PathBeneath::new(
            PathFd::new("/").ok()?,
            AccessFs::from_read(abi),
        ))
        .ok()?;
    // Full access under cwd, tmp, /dev.
    for p in [cwd.as_path(), tmp.as_path(), std::path::Path::new("/dev")] {
        if let Ok(fd) = PathFd::new(p) {
            ruleset = ruleset
                .add_rule(landlock::PathBeneath::new(fd, AccessFs::from_all(abi)))
                .ok()?;
        }
    }
    // Proxy mode: the single connectable TCP port. Off adds no ports.
    if let EgressState::Proxy(port) = egress {
        ruleset = ruleset
            .add_rule(
                NetPort::new(*port, AccessNet::ConnectTcp)
                    .set_compatibility(CompatLevel::HardRequirement),
            )
            .ok()?;
    }
    // Extract the ruleset fd; None when the kernel can't enforce it.
    ruleset.into()
}

/// Wire the Landlock ruleset onto `cmd` via `pre_exec`. Shared tail of
/// `landlock_command`/`landlock_argv` — only how `cmd` is constructed
/// (`sh -c` vs direct argv) differs between the two.
#[cfg(target_os = "linux")]
fn apply_landlock(
    mut cmd: tokio::process::Command,
    egress: &EgressState,
) -> tokio::process::Command {
    use std::os::unix::io::{AsRawFd, OwnedFd};

    // The OwnedFd is captured by the closure, so it stays open across every
    // spawn of this Command (pre_exec runs after fork, before exec — a
    // parent-owned fd is still open in the child there). Fail-closed: with
    // no usable fd the child refuses to exec rather than run unconfined.
    let ruleset_fd: Option<OwnedFd> = build_landlock_ruleset(egress);
    let apply = move || {
        // Async-signal-safe only from here: raw syscalls, no allocation.
        let Some(fd) = ruleset_fd.as_ref().map(|f| f.as_raw_fd()) else {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
        };
        // SAFETY: plain syscalls with no memory handed to the kernel beyond
        // the fd and integer flags.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    };
    // SAFETY: `apply` performs only async-signal-safe operations (see above).
    unsafe {
        cmd.pre_exec(apply);
    }
    cmd
}

#[cfg(target_os = "linux")]
fn landlock_command(command: &str, egress: &EgressState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    apply_landlock(cmd, egress)
}

#[cfg(target_os = "linux")]
fn landlock_argv(program: &str, args: &[String], egress: &EgressState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    apply_landlock(cmd, egress)
}

#[cfg(test)]
mod env_tests {
    use super::*;

    /// The proxy URL a child sees. Since Task 9 it carries the session
    /// credential as userinfo (curl/git/pip/cargo forward that as
    /// `Proxy-Authorization`), so this asserts the *shape* — right authority,
    /// and a non-empty credential when auth is on — rather than re-deriving
    /// the token, which would make the assertion a tautology.
    fn assert_proxy_url(envs: &[(String, Option<String>)], key: &str, port: u16) {
        let value = envs
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{key} must be set"));
        let authority = format!("@127.0.0.1:{port}");
        match crate::net::proxy_user_info() {
            Some(_) => {
                assert!(
                    value.ends_with(&authority),
                    "{key} must point at the proxy: {value}"
                );
                let userinfo = value
                    .strip_prefix("http://")
                    .and_then(|r| r.strip_suffix(&authority))
                    .unwrap_or_default();
                let secret = userinfo.strip_prefix("hotl:").unwrap_or_default();
                assert!(
                    !secret.is_empty(),
                    "{key} must carry a non-empty session credential: {value}"
                );
            }
            None => assert_eq!(value, format!("http://127.0.0.1:{port}")),
        }
    }

    #[test]
    fn proxy_state_injects_proxy_env_and_off_does_not() {
        // Env injection is OS-independent (it rides the Command itself).
        let status = SandboxStatus::Unavailable("test".into());
        let cmd = build_command("true", &status, &EgressState::Proxy(9123));
        let envs: Vec<_> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
        ] {
            assert_proxy_url(&envs, key, 9123);
        }
        for key in ["NO_PROXY", "no_proxy"] {
            assert!(
                envs.contains(&(key.to_string(), Some("localhost,127.0.0.1,::1".to_string()))),
                "{key} must exempt loopback"
            );
        }
        // Off and Open *set* nothing (kernel-only / unchanged behavior). The
        // environment is not untouched, though: `apply_child_env` removes the
        // provider credentials on every path — pinned by
        // `provider_credentials_never_reach_the_child` below.
        for egress in [EgressState::Off, EgressState::Open] {
            let cmd = build_command("true", &status, &egress);
            assert!(
                cmd.as_std().get_envs().all(|(_, v)| v.is_none()),
                "{egress:?} must only remove variables, never set one"
            );
        }
    }

    #[test]
    fn provider_credentials_never_reach_the_child() {
        let status = SandboxStatus::Unavailable("test".into());
        for egress in [
            EgressState::Open,
            EgressState::Off,
            EgressState::Proxy(9123),
        ] {
            let cmd = build_command("true", &status, &egress);
            let envs: Vec<(String, Option<String>)> = cmd
                .as_std()
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            for name in [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "OPENAI_API_KEY",
                "HOTL_API_KEY_HELPER",
            ] {
                assert!(
                    envs.contains(&(name.to_string(), None)),
                    "{name} must be removed from the child env under {egress:?}"
                );
            }
            // A credential a child legitimately needs is left alone by default.
            assert!(
                !envs.iter().any(|(k, _)| k == "GITHUB_TOKEN"),
                "the default scrub must not strip GITHUB_TOKEN (gh/git would break)"
            );
        }
        // The same chokepoint covers the argv shape.
        let argv = build_argv("rg", &["--files".to_string()], &status, &EgressState::Open);
        assert!(argv
            .as_std()
            .get_envs()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_none()));
    }

    #[test]
    fn strict_mode_extends_the_scrub_and_explicit_names_add_to_it() {
        assert!(names_to_scrub(Some("MY_THING,OTHER"), false)
            .iter()
            .any(|n| n == "MY_THING"));
        assert!(names_to_scrub(None, false)
            .iter()
            .any(|n| n == "ANTHROPIC_API_KEY"));
        // Strict adds every secret-named var *present in this process*.
        std::env::set_var("HOTL_TEST_SCRUB_TOKEN", "abcdefghij");
        assert!(names_to_scrub(None, true)
            .iter()
            .any(|n| n == "HOTL_TEST_SCRUB_TOKEN"));
        assert!(!names_to_scrub(None, false)
            .iter()
            .any(|n| n == "HOTL_TEST_SCRUB_TOKEN"));
        std::env::remove_var("HOTL_TEST_SCRUB_TOKEN");
    }

    #[test]
    fn build_argv_execs_program_and_args_directly_and_still_wires_proxy_env() {
        // No sandbox floor on this "host": the argv falls through to a plain
        // `Command::new(program)` — no `sh -c` wrapping, ever.
        let status = SandboxStatus::Unavailable("test".into());
        let args = vec!["--line-number".to_string(), "needle".to_string()];
        let cmd = build_argv("rg", &args, &status, &EgressState::Open);
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "rg");
        let got_args: Vec<_> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got_args, args);

        // Proxy env wiring is shared with build_command.
        let proxied = build_argv("rg", &args, &status, &EgressState::Proxy(9123));
        let envs: Vec<_> = proxied
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_proxy_url(&envs, "HTTP_PROXY", 9123);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    async fn run(cmd: &str) -> std::process::Output {
        run_with(cmd, &EgressState::Open).await
    }

    async fn run_with(cmd: &str, egress: &EgressState) -> std::process::Output {
        let status = probe();
        assert!(
            matches!(status, SandboxStatus::Enforced("seatbelt")),
            "no seatbelt on this mac?"
        );
        build_command(cmd, &status, egress)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .expect("spawn")
    }

    /// Replaces `seatbelt_profile_strings_do_not_drift`, which asserted a
    /// literal equals itself — a pure change detector proving nothing about
    /// enforcement. This asserts the *properties* the profile must have; the
    /// behavioral tests below and `seatbelt_confines_writes` prove enforcement.
    #[test]
    fn seatbelt_profile_carries_every_required_clause() {
        let open = seatbelt_profile(false, UnixSockets::DenyDaemons);
        for clause in [
            "(deny file-write*)",
            "(subpath (param \"CWD\"))",
            "(subpath (param \"TMP\"))",
            "docker",
        ] {
            assert!(open.contains(clause), "profile lost `{clause}`:\n{open}");
        }
        let confined = seatbelt_profile(true, UnixSockets::DenyDaemons);
        for clause in [
            "(deny network*)",
            "(allow network* (local unix) (remote unix))",
        ] {
            assert!(
                confined.contains(clause),
                "confined profile lost `{clause}`"
            );
        }
        // Network confinement is additive over the open profile's file clauses.
        assert!(confined.contains("(deny file-write*)"));
    }

    #[test]
    fn seatbelt_denies_the_container_daemon_socket_class_in_both_network_modes() {
        for confined in [false, true] {
            let p = seatbelt_profile(confined, UnixSockets::DenyDaemons);
            // The first *deny* on network-outbound: the `(allow
            // network-outbound (remote ip …))` of the confined profile also
            // contains the operation name, so match on the deny itself.
            let deny = p
                .find("(deny network-outbound")
                .expect("a unix-socket deny clause");
            // The clause is a regex, so `docker` and `.sock` are not adjacent
            // in the profile text — assert both halves of the alternation.
            assert!(
                p[deny..].contains("docker") && p[deny..].contains(".sock"),
                "the docker daemon socket class must be denied:\n{p}"
            );
            if confined {
                // Keep the denies terminal: they are appended after the
                // re-allow so no later clause can widen them back.
                assert!(
                    deny > p.find("(allow network* (local unix)").unwrap(),
                    "the daemon deny must follow the unix re-allow"
                );
            }
        }
        // The opt-out really opts out.
        assert!(!seatbelt_profile(false, UnixSockets::Open).contains("docker"));
    }

    #[tokio::test]
    async fn seatbelt_blocks_a_connect_to_a_denied_daemon_socket() {
        // A real unix socket at a denied path, in a temp dir we control, so the
        // test proves the *deny*, not the absence of Docker.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("docker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        // Positive control: a *differently named* socket in the same dir connects.
        let allowed = dir.path().join("ok.sock");
        let l2 = std::os::unix::net::UnixListener::bind(&allowed).unwrap();
        std::thread::spawn(move || {
            let _ = l2.accept();
        });

        let connect = |p: &std::path::Path| format!("nc -U {} < /dev/null", p.display());
        let denied = run(&connect(&sock)).await;
        let ok = run(&connect(&allowed)).await;
        assert!(
            ok.status.success(),
            "the control socket must still connect: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert!(
            !denied.status.success(),
            "a *.docker.sock connect must be denied"
        );
    }

    #[test]
    fn label_marks_only_the_opted_out_state() {
        assert_eq!(
            SandboxStatus::Enforced("seatbelt").label(),
            "sandboxed:seatbelt"
        );
        // With the opt-out active the marker rides every ask; here assert the
        // pure renderer.
        assert_eq!(
            label_with("seatbelt", UnixSockets::Open, Automation::Allow),
            "sandboxed:seatbelt unix:open automation:allow"
        );
    }

    #[tokio::test]
    async fn seatbelt_egress_off_confines_to_loopback() {
        // A loopback listener the confined command may reach.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        // Loopback connect: allowed under egress Off.
        let ok = run_with(&format!("nc -z -G 2 127.0.0.1 {port}"), &EgressState::Off).await;
        assert!(
            ok.status.success(),
            "loopback connect should be allowed under egress off: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        // Outbound non-loopback connect: must not succeed. (On an offline
        // machine it also fails — that is the safe direction to assert.)
        let denied = run_with("nc -z -G 2 1.1.1.1 443", &EgressState::Off).await;
        assert!(
            !denied.status.success(),
            "non-loopback connect must fail under egress off"
        );
    }

    #[tokio::test]
    async fn seatbelt_confines_writes() {
        // Write inside cwd: allowed.
        let inside = format!(
            "touch ./.hotl-sbx-ok-{} && rm ./.hotl-sbx-ok-{}",
            std::process::id(),
            std::process::id()
        );
        let ok = run(&inside).await;
        assert!(
            ok.status.success(),
            "cwd write should be allowed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );

        // Write outside (HOME): denied by the floor.
        let home = std::env::var("HOME").expect("HOME");
        let target = format!("{home}/.hotl-sbx-denied-{}", std::process::id());
        let outside = format!("touch {target}");
        let denied = run(&outside).await;
        let leaked = std::path::Path::new(&target).exists();
        if leaked {
            std::fs::remove_file(&target).ok();
        }
        assert!(
            !denied.status.success(),
            "write outside cwd must fail under the floor"
        );
        assert!(!leaked, "file must not exist outside the sandbox");

        // Reads outside stay allowed (floor is write-confinement).
        let read = run(&format!("ls {home} > /dev/null")).await;
        assert!(read.status.success(), "reads outside cwd should be allowed");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    async fn run(cmd: &str) -> std::process::Output {
        let status = probe();
        assert!(
            matches!(status, SandboxStatus::Enforced(_)),
            "no landlock here? {status:?}"
        );
        build_command(cmd, &status, &EgressState::Open)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .expect("spawn")
    }

    /// `seatbelt_confines_writes` had no Linux twin. This is it.
    #[tokio::test]
    async fn landlock_confines_writes() {
        let dir = probe_dir().expect("a probe dir outside the confinement");
        let target = dir.join(format!("hotl-ll-write-{}", std::process::id()));
        let _ = std::fs::remove_file(&target);

        // Inside cwd: allowed.
        let ok = run(&format!(
            "touch ./.hotl-ll-ok-{0} && rm ./.hotl-ll-ok-{0}",
            std::process::id()
        ))
        .await;
        assert!(
            ok.status.success(),
            "cwd write must be allowed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );

        // Outside: denied, and nothing lands.
        let denied = run(&format!("echo x > {}", target.display())).await;
        let leaked = target.exists();
        if leaked {
            let _ = std::fs::remove_file(&target);
        }
        assert!(
            !denied.status.success(),
            "write outside cwd must fail under landlock"
        );
        assert!(!leaked, "file must not exist outside the sandbox");

        // Reads outside stay allowed (the floor is write-confinement).
        assert!(run("ls / > /dev/null").await.status.success());
    }

    /// A shell word that calls `truncate(2)` **by path** on `victim`.
    ///
    /// The demonstrator has to reach the raw syscall. Neither `truncate -s 0`
    /// (GNU coreutils `open`s the file `O_WRONLY` first) nor `> file` (open
    /// with `O_CREAT|O_TRUNC`) will do: `AccessFs::WriteFile` already denies
    /// both at ABI v1, so they are denied whether or not the truncate right is
    /// handled and prove nothing either way. Measured against the v2 ruleset:
    /// coreutils and the redirect are denied with EACCES while a raw
    /// `truncate(2)` zeroes the file. `perl` first, `python3` as the fallback;
    /// with neither the test fails loudly rather than skipping (a skip here
    /// would be indistinguishable from enforcement).
    fn truncate_by_path(victim: &std::path::Path) -> String {
        let p = victim.display();
        if std::path::Path::new("/usr/bin/perl").exists() {
            return format!("perl -e 'truncate($ARGV[0], 0) or die $!' {p}");
        }
        format!("python3 -c \"import os,sys; os.truncate(sys.argv[1], 0)\" {p}")
    }

    /// The verified hole: `truncate(2)` by path is a *v3* right and the
    /// ruleset only handled v2, so this succeeded on every kernel.
    #[tokio::test]
    async fn landlock_confines_truncate_by_path() {
        if !matches!(probe(), SandboxStatus::Enforced("landlock")) {
            // A `landlock(partial)` host genuinely cannot enforce this; the
            // label says so. Assert *that*, rather than skipping silently.
            assert!(
                landlock_abi().is_some_and(|a| (a as i32) < 3),
                "v3+ host must certify fully"
            );
            return;
        }
        let dir = probe_dir().expect("a probe dir");
        let victim = dir.join(format!("hotl-ll-trunc-{}", std::process::id()));
        std::fs::write(&victim, b"important").unwrap();
        let out = run(&truncate_by_path(&victim)).await;
        let size = std::fs::metadata(&victim).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(&victim);
        assert!(
            !out.status.success(),
            "truncate outside cwd must be denied: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(size, 9, "the file outside the workspace must be untouched");
    }

    #[test]
    fn abi_below_v3_is_not_silently_certified() {
        // Whatever this kernel is, the verdict and the ABI must agree.
        match (probe(), landlock_abi()) {
            (SandboxStatus::Enforced("landlock"), Some(abi)) => assert!(abi as i32 >= 3),
            (SandboxStatus::Enforced("landlock(partial)"), Some(abi)) => {
                assert!((abi as i32) < 3);
                assert_eq!(std::env::var("HOTL_SANDBOX").as_deref(), Ok("best-effort"));
            }
            (SandboxStatus::Unavailable(reason), Some(abi)) => {
                assert!((abi as i32) < 3 && reason.contains("truncate"), "{reason}");
            }
            (status, abi) => panic!("inconsistent: {status:?} / {abi:?}"),
        }
    }
}
