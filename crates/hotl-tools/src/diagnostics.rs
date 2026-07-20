//! Post-mutation diagnostics (M3a; corpus 05's format+diagnostics injection,
//! config-driven — no LSP until the ledger row justifies one).
//!
//! `~/.config/hotl/hooks.toml` maps file extensions to a check command:
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

const TIMEOUT_SECS: u64 = 30;
const MAX_REPORT_LINES: usize = 30;

#[derive(Default)]
pub struct Diagnostics {
    /// extension → shell command
    commands: HashMap<String, String>,
}

impl Diagnostics {
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join("hooks.toml");
        let commands = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| raw.parse::<toml::Table>().ok())
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
    pub async fn check(&self, path: &str) -> Option<String> {
        let ext = Path::new(path).extension()?.to_str()?;
        let command = self.commands.get(ext)?;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(TIMEOUT_SECS),
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await;
        let report = match output {
            Err(_) => format!("(check `{command}` timed out after {TIMEOUT_SECS}s)"),
            Ok(Err(e)) => format!("(check `{command}` could not run: {e})"),
            Ok(Ok(out)) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                let head: Vec<&str> = text.lines().take(MAX_REPORT_LINES).collect();
                if out.status.success() && head.is_empty() {
                    return None;
                }
                let status = if out.status.success() { "clean" } else { "FAILED" };
                format!("{status} — `{command}`\n{}", head.join("\n"))
            }
        };
        Some(format!("\n<diagnostics>\n{report}\n</diagnostics>"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(toml: &str) -> Diagnostics {
        let dir = std::env::temp_dir().join(format!("hotl-diag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hooks.toml"), toml).unwrap();
        let d = Diagnostics::load(&dir);
        std::fs::remove_dir_all(&dir).ok();
        d
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
}
