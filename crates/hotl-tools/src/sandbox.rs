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

/// Probe what this host can enforce.
pub fn probe() -> SandboxStatus {
    if std::env::var("HOTL_SANDBOX").is_ok_and(|v| v == "off") {
        return SandboxStatus::Disabled;
    }
    #[cfg(target_os = "macos")]
    {
        if std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            return SandboxStatus::Enforced("seatbelt");
        }
        return SandboxStatus::Unavailable("sandbox-exec not found".into());
    }
    #[cfg(target_os = "linux")]
    {
        use landlock::{Access, AccessFs, Ruleset, RulesetAttr, ABI};
        // Creating (not applying) a ruleset probes kernel support.
        match Ruleset::default().handle_access(AccessFs::from_all(ABI::V2)) {
            Ok(r) => match r.create() {
                Ok(_) => return SandboxStatus::Enforced("landlock"),
                Err(e) => return SandboxStatus::Unavailable(format!("landlock unavailable: {e}")),
            },
            Err(e) => return SandboxStatus::Unavailable(format!("landlock unavailable: {e}")),
        }
    }
    #[allow(unreachable_code)]
    SandboxStatus::Unavailable("no sandbox mechanism for this OS".into())
}

/// Build the command for `sh -c <command>` under the active floor and the
/// resolved egress state. With `EgressState::Open` the result is byte-identical
/// to the pre-egress behavior.
pub fn build_command(
    command: &str,
    status: &SandboxStatus,
    egress: &EgressState,
) -> tokio::process::Command {
    let mut cmd = match status {
        SandboxStatus::Enforced("seatbelt") => seatbelt_command(command, egress),
        #[cfg(target_os = "linux")]
        SandboxStatus::Enforced("landlock") => landlock_command(command, egress),
        _ => {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    };
    // Allowlist mode: cooperating clients (curl, git, pip, cargo — anything
    // honoring the proxy env) route through the filtering proxy on loopback;
    // non-cooperating clients hit the kernel loopback-only wall and fail
    // closed. For Off, no env — the kernel does all the work.
    if let EgressState::Proxy(port) = egress {
        let proxy = format!("http://127.0.0.1:{port}");
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY"] {
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

#[cfg(target_os = "macos")]
fn seatbelt_command(command: &str, egress: &EgressState) -> tokio::process::Command {
    let confine_network = matches!(egress, EgressState::Off | EgressState::Proxy(_));
    let cwd = canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let tmp = canon(std::env::temp_dir());
    let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-p")
        .arg(seatbelt_profile(confine_network))
        .arg("-D")
        .arg(format!("CWD={}", cwd.display()))
        .arg("-D")
        .arg(format!("TMP={}", tmp.display()))
        .arg("sh")
        .arg("-c")
        .arg(command);
    cmd
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn seatbelt_command(command: &str, _egress: &EgressState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
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

#[cfg(target_os = "linux")]
fn landlock_command(command: &str, egress: &EgressState) -> tokio::process::Command {
    use std::os::unix::io::{AsRawFd, OwnedFd};

    use landlock::{
        Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr, ABI,
    };

    /// Build the fully-populated ruleset **in the parent**: `pre_exec` runs
    /// between fork and exec in a multithreaded process, where allocation
    /// (malloc lock) and other non-async-signal-safe work can deadlock, so
    /// everything that allocates happens here, before the spawn.
    ///
    /// The fs part stays best-effort at ABI::V2 as before. The net part
    /// (Off/Proxy egress) is a **hard requirement**: `ConnectTcp` only —
    /// zero allowed ports for Off, exactly the proxy port for Proxy. Two
    /// honest Linux limits, by Landlock's design: net rules are **TCP-only**
    /// (UDP — including DNS and DNS-tunnel exfiltration — is not confined)
    /// and **port-scoped, not address-scoped** (the proxy port number is
    /// connectable on any host, and Off blocks loopback TCP too; unix-domain
    /// sockets are untouched either way).
    fn build_ruleset(egress: &EgressState) -> Option<OwnedFd> {
        let abi = ABI::V2;
        let cwd = canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let tmp = canon(std::env::temp_dir());
        let confine_network = matches!(egress, EgressState::Off | EgressState::Proxy(_));
        let mut attr = Ruleset::default().handle_access(AccessFs::from_all(abi)).ok()?;
        if confine_network {
            // HardRequirement: on a kernel without the net ABI this fails,
            // build_ruleset returns None, and the child refuses to exec
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
            .add_rule(landlock::PathBeneath::new(PathFd::new("/").ok()?, AccessFs::from_read(abi)))
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

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    // The OwnedFd is captured by the closure, so it stays open across every
    // spawn of this Command (pre_exec runs after fork, before exec — a
    // parent-owned fd is still open in the child there). Fail-closed: with
    // no usable fd the child refuses to exec rather than run unconfined.
    let ruleset_fd: Option<OwnedFd> = build_ruleset(egress);
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
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect();
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY"] {
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
            assert_eq!(cmd.as_std().get_envs().count(), 0, "{egress:?} must not touch env");
        }
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
        assert!(matches!(status, SandboxStatus::Enforced("seatbelt")), "no seatbelt on this mac?");
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
        assert!(confined.starts_with(&seatbelt_profile(false)), "file-write clauses unchanged");
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
        assert!(!denied.status.success(), "non-loopback connect must fail under egress off");
    }

    #[tokio::test]
    async fn seatbelt_confines_writes() {
        // Write inside cwd: allowed.
        let inside = format!("touch ./.hotl-sbx-ok-{} && rm ./.hotl-sbx-ok-{}", std::process::id(), std::process::id());
        let ok = run(&inside).await;
        assert!(ok.status.success(), "cwd write should be allowed: {}", String::from_utf8_lossy(&ok.stderr));

        // Write outside (HOME): denied by the floor.
        let home = std::env::var("HOME").expect("HOME");
        let target = format!("{home}/.hotl-sbx-denied-{}", std::process::id());
        let outside = format!("touch {target}");
        let denied = run(&outside).await;
        let leaked = std::path::Path::new(&target).exists();
        if leaked {
            std::fs::remove_file(&target).ok();
        }
        assert!(!denied.status.success(), "write outside cwd must fail under the floor");
        assert!(!leaked, "file must not exist outside the sandbox");

        // Reads outside stay allowed (floor is write-confinement).
        let read = run(&format!("ls {home} > /dev/null")).await;
        assert!(read.status.success(), "reads outside cwd should be allowed");
    }
}
