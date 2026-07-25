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

impl SandboxStatus {
    pub fn label(&self) -> String {
        match self {
            SandboxStatus::Enforced(m) => format!("sandboxed:{m}"),
            SandboxStatus::Unavailable(_) => "UNSANDBOXED".to_string(),
            SandboxStatus::Disabled => "UNSANDBOXED(by HOTL_SANDBOX=off)".to_string(),
        }
    }
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
    apply_proxy_env(cmd, egress)
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
        let proxy = format!("http://127.0.0.1:{port}");
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
fn seatbelt_profile(confine_network: bool) -> String {
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
        .arg(seatbelt_profile(confine_network))
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
            assert!(
                envs.contains(&(key.to_string(), Some("http://127.0.0.1:9123".to_string()))),
                "{key} must point at the proxy"
            );
        }
        for key in ["NO_PROXY", "no_proxy"] {
            assert!(
                envs.contains(&(key.to_string(), Some("localhost,127.0.0.1,::1".to_string()))),
                "{key} must exempt loopback"
            );
        }
        // Off and Open set nothing (kernel-only / unchanged behavior).
        for egress in [EgressState::Off, EgressState::Open] {
            let cmd = build_command("true", &status, &egress);
            assert_eq!(
                cmd.as_std().get_envs().count(),
                0,
                "{egress:?} must not touch env"
            );
        }
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
        assert!(envs.contains(&(
            "HTTP_PROXY".to_string(),
            Some("http://127.0.0.1:9123".to_string())
        )));
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

    #[test]
    fn seatbelt_profile_strings_do_not_drift() {
        // Open: exactly the pre-egress profile, byte for byte.
        assert_eq!(
            seatbelt_profile(false),
            r#"(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (subpath (param "CWD"))
  (subpath (param "TMP"))
  (subpath "/private/tmp")
  (subpath "/dev"))
"#
        );
        // Confined: the same file-write clauses plus network confinement —
        // deny all, re-allow unix-domain sockets and loopback.
        let confined = seatbelt_profile(true);
        assert!(
            confined.starts_with(&seatbelt_profile(false)),
            "file-write clauses unchanged"
        );
        assert_eq!(
            confined.strip_prefix(&seatbelt_profile(false)).unwrap(),
            r#"(deny network*)
(allow network* (local unix) (remote unix))
(allow network-outbound (remote ip "localhost:*"))
(allow network-inbound (local ip "localhost:*"))
"#
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
