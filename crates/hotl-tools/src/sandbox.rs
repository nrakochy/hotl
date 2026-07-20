//! The kernel sandbox floor for `bash` (SECURITY.md layer 3).
//!
//! Write-confinement: the command (and its whole process tree) can read
//! everywhere but write only under the working directory, the temp dir, and
//! /dev. Network stays open in v1 (the agent legitimately curls); tightening
//! is an M5 policy decision.
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

/// Build the command for `sh -c <command>` under the active floor.
pub fn build_command(command: &str, status: &SandboxStatus) -> tokio::process::Command {
    match status {
        SandboxStatus::Enforced("seatbelt") => seatbelt_command(command),
        #[cfg(target_os = "linux")]
        SandboxStatus::Enforced("landlock") => landlock_command(command),
        _ => {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    }
}

#[cfg(target_os = "macos")]
fn seatbelt_command(command: &str) -> tokio::process::Command {
    // Write-deny by default; allow the working tree, temp, and /dev.
    const PROFILE: &str = r#"(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (subpath (param "CWD"))
  (subpath (param "TMP"))
  (subpath "/private/tmp")
  (subpath "/dev"))
"#;
    let cwd = canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let tmp = canon(std::env::temp_dir());
    let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-p")
        .arg(PROFILE)
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
fn seatbelt_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(target_os = "linux")]
fn landlock_command(command: &str) -> tokio::process::Command {
    use landlock::{
        Access, AccessFs, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
    };
    let cwd = canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let tmp = canon(std::env::temp_dir());
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    unsafe {
        cmd.pre_exec(move || {
            let abi = ABI::V2;
            let apply = || -> Result<(), Box<dyn std::error::Error>> {
                let mut ruleset = Ruleset::default().handle_access(AccessFs::from_all(abi))?.create()?;
                // Read + execute everywhere.
                ruleset = ruleset.add_rule(landlock::PathBeneath::new(
                    PathFd::new("/")?,
                    AccessFs::from_read(abi),
                ))?;
                // Full access under cwd, tmp, /dev.
                for p in [cwd.as_path(), tmp.as_path(), std::path::Path::new("/dev")] {
                    if let Ok(fd) = PathFd::new(p) {
                        ruleset = ruleset
                            .add_rule(landlock::PathBeneath::new(fd, AccessFs::from_all(abi)))?;
                    }
                }
                ruleset.restrict_self()?;
                Ok(())
            };
            apply().map_err(|e| std::io::Error::other(format!("landlock: {e}")))
        });
    }
    cmd
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    async fn run(cmd: &str) -> std::process::Output {
        let status = probe();
        assert!(matches!(status, SandboxStatus::Enforced("seatbelt")), "no seatbelt on this mac?");
        build_command(cmd, &status)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .expect("spawn")
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
