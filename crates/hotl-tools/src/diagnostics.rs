//! Post-mutation diagnostics (M3a; format+diagnostics injection,
//! config-driven — no LSP for now).
//!
//! The `[diagnostics]` table of `~/.config/hotl/config.toml` maps file
//! extensions to a check command:
//!
//! ```toml
//! [diagnostics]
//! rs = "cargo check -q --message-format=short"
//! py = "ruff check ."
//! ```
//!
//! After a successful `edit`/`write`, the matching command runs (in the
//! session's working directory, timeout-bounded) and its head is appended to
//! the tool result — the model sees breakage in the same step that caused it.

use std::collections::HashMap;
use std::path::Path;

use crate::sandbox;

const TIMEOUT_SECS: u64 = 30;
const MAX_REPORT_LINES: usize = 30;

#[derive(Default)]
pub struct Diagnostics {
    /// extension → shell command
    commands: HashMap<String, String>,
}

impl Diagnostics {
    /// Parse the `[diagnostics]` table from a TOML string (the binary feeds the
    /// relevant slice of config.toml).
    pub fn from_toml(text: &str) -> Self {
        let commands = text
            .parse::<toml::Table>()
            .ok()
            .and_then(|t| t.get("diagnostics").cloned())
            .and_then(|d| d.as_table().cloned())
            .map(|table| {
                table
                    .into_iter()
                    .filter_map(|(ext, v)| v.as_str().map(|c| (ext, c.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Self { commands }
    }

    /// Run the configured check for `path`'s extension. `Some(report)` when
    /// the command failed or produced output — silence means clean.
    ///
    /// The command runs under the **same sandbox floor as `bash`** (H-11): a
    /// diagnostic like `cargo check` compiles workspace `build.rs`/proc-macros
    /// the model just wrote, so it must be write-confined too, not a hole in
    /// the floor. It is also process-group-killed on timeout so a runaway
    /// `cargo`/`rustc` subtree can't be orphaned.
    pub async fn check(&self, path: &str) -> Option<String> {
        let ext = Path::new(path).extension()?.to_str()?;
        let command = self.commands.get(ext)?;
        let report = match self.run(command).await {
            Outcome::TimedOut => format!("(check `{command}` timed out after {TIMEOUT_SECS}s)"),
            Outcome::Failed(e) => format!("(check `{command}` could not run: {e})"),
            Outcome::Ran { text, success } => {
                let head: Vec<&str> = text.lines().take(MAX_REPORT_LINES).collect();
                if success && head.is_empty() {
                    return None;
                }
                let status = if success { "clean" } else { "FAILED" };
                format!("{status} — `{command}`\n{}", head.join("\n"))
            }
        };
        Some(format!("\n<diagnostics>\n{report}\n</diagnostics>"))
    }

    async fn run(&self, command: &str) -> Outcome {
        let status = sandbox::probe();
        let mut cmd = sandbox::build_command(command, &status);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let pid = child.id();
        let wait = child.wait_with_output();
        tokio::pin!(wait);
        tokio::select! {
            result = &mut wait => match result {
                Ok(out) => {
                    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                    Outcome::Ran { text, success: out.status.success() }
                }
                Err(e) => Outcome::Failed(e.to_string()),
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(TIMEOUT_SECS)) => {
                crate::builtins::kill_group(pid);
                Outcome::TimedOut
            }
        }
    }
}

enum Outcome {
    Ran { text: String, success: bool },
    TimedOut,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(toml: &str) -> Diagnostics {
        Diagnostics::from_toml(toml)
    }

    #[tokio::test]
    async fn reports_failures_and_stays_silent_when_clean() {
        let d = diag("[diagnostics]\nfoo = \"echo broken; exit 1\"\nbar = \"true\"\n");
        let report = d.check("src/thing.foo").await.expect("failure reported");
        assert!(report.contains("FAILED") && report.contains("broken"));
        assert!(d.check("src/thing.bar").await.is_none(), "clean + quiet = silent");
        assert!(d.check("src/thing.unknown").await.is_none(), "unconfigured ext = silent");
        assert!(d.check("no-extension").await.is_none());
    }

    #[tokio::test]
    async fn runs_under_the_sandbox_floor() {
        // On a host with an enforced floor, a diagnostic that writes outside
        // cwd must be confined exactly like bash (H-11). Where no floor exists
        // the command still runs (behind the write ask upstream) — so this
        // only asserts the confinement when the floor is actually enforced.
        use crate::sandbox::SandboxStatus;
        if !matches!(sandbox::probe(), SandboxStatus::Enforced(_)) {
            return;
        }
        // Home is outside the cwd/tmp/dev write set the floor permits.
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return };
        let outside = home.join(format!(".hotl-diag-escape-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        let d = diag(&format!(
            "[diagnostics]\nfoo = \"echo x > {} 2>/dev/null; true\"\n",
            outside.display()
        ));
        // The write is outside cwd/tmp confinement → blocked; command still
        // returns, and the escape file must not exist.
        let _ = d.check("src/thing.foo").await;
        assert!(!outside.exists(), "diagnostic escaped the sandbox floor");
    }
}
