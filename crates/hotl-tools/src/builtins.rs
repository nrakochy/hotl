//! The four M0 built-ins. Failure messages are prompts (they instruct the
//! model); truncation carries continuation hints.

use crate::sandbox::{self, SandboxStatus};
use crate::{execute_later_reason, Permission, Tool, ToolOutcome};
use std::sync::OnceLock;

fn sandbox_status() -> &'static SandboxStatus {
    static STATUS: OnceLock<SandboxStatus> = OnceLock::new();
    STATUS.get_or_init(sandbox::probe)
}
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const READ_MAX_BYTES: usize = 200 * 1024;
const READ_MAX_LINES: usize = 2000;
const BASH_DEFAULT_TIMEOUT_MS: u64 = 120_000;
const BASH_MAX_TIMEOUT_MS: u64 = 600_000;
const BASH_MAX_OUTPUT: usize = 50 * 1024;

fn str_arg<'v>(input: &'v Value, key: &str) -> Result<&'v str, ToolOutcome> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| {
        ToolOutcome::err(format!("Missing required string argument `{key}`. Re-send the call with `{key}` set."))
    })
}

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read a text file from the local filesystem. Returns at most 2000 lines / 200KB per call; use `offset` (1-indexed start line) to continue a truncated read."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path (absolute or relative to the working directory)"},
                "offset": {"type": "integer", "description": "1-indexed line to start from (for continuing truncated reads)"}
            },
            "required": ["path"]
        })
    }
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let path = match str_arg(&input, "path") {
                Ok(p) => p.to_string(),
                Err(e) => return e,
            };
            let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => {
                    return ToolOutcome::err(format!(
                        "Could not read `{path}`: {e}. Check the path (use `bash` with `ls` to explore) and try again."
                    ))
                }
            };
            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len();
            if offset > total && total > 0 {
                return ToolOutcome::err(format!(
                    "`{path}` has only {total} lines; offset {offset} is past the end."
                ));
            }
            let mut out = String::new();
            for (i, line) in lines.iter().enumerate().skip(offset - 1) {
                let taken = i - (offset - 1);
                if taken >= READ_MAX_LINES || out.len() + line.len() > READ_MAX_BYTES {
                    out.push_str(&format!(
                        "\n[truncated: showing lines {offset}-{} of {total}; continue with offset={}]",
                        i, i + 1
                    ));
                    break;
                }
                out.push_str(&format!("{:>6}\t{line}\n", i + 1));
            }
            if out.is_empty() {
                out = "[empty file]".into();
            }
            ToolOutcome::ok(out)
        })
    }
}

pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Write a file (creating parent directories), overwriting any existing content. For partial changes to an existing file prefer `edit`."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }
    fn permission(&self, input: &Value) -> Permission {
        let path = input.get("path").and_then(Value::as_str).unwrap_or("?");
        let summary = format!("write {path}");
        match execute_later_reason(path) {
            Some(why) => Permission::AskProtected { summary, why: why.into() },
            None => Permission::Ask { summary },
        }
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let path = match str_arg(&input, "path") {
                Ok(p) => p.to_string(),
                Err(e) => return e,
            };
            let content = match str_arg(&input, "content") {
                Ok(c) => c.to_string(),
                Err(e) => return e,
            };
            if let Some(parent) = std::path::Path::new(&path).parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        return ToolOutcome::err(format!("Could not create parent directories for `{path}`: {e}."));
                    }
                }
            }
            match tokio::fs::write(&path, &content).await {
                Ok(()) => ToolOutcome::ok(format!("Wrote {} bytes to {path}.", content.len())),
                Err(e) => ToolOutcome::err(format!("Could not write `{path}`: {e}.")),
            }
        })
    }
}

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "Exact string replacement in a file. `old_string` must match exactly once, including whitespace; include surrounding lines to make it unique."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    fn permission(&self, input: &Value) -> Permission {
        let path = input.get("path").and_then(Value::as_str).unwrap_or("?");
        let summary = format!("edit {path}");
        match execute_later_reason(path) {
            Some(why) => Permission::AskProtected { summary, why: why.into() },
            None => Permission::Ask { summary },
        }
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let path = match str_arg(&input, "path") {
                Ok(p) => p.to_string(),
                Err(e) => return e,
            };
            let old = match str_arg(&input, "old_string") {
                Ok(s) => s.to_string(),
                Err(e) => return e,
            };
            let new = match str_arg(&input, "new_string") {
                Ok(s) => s.to_string(),
                Err(e) => return e,
            };
            if old.is_empty() {
                return ToolOutcome::err("`old_string` is empty. Use `write` to create a file, or provide the exact text to replace.".to_string());
            }
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => return ToolOutcome::err(format!("Could not read `{path}`: {e}. Read the file first to confirm the path.")),
            };
            let count = content.matches(&old).count();
            match count {
                0 => ToolOutcome::err(format!(
                    "`old_string` was not found in `{path}`. Read the file and copy the exact text, including whitespace and indentation."
                )),
                1 => {
                    let updated = content.replacen(&old, &new, 1);
                    match tokio::fs::write(&path, updated).await {
                        Ok(()) => ToolOutcome::ok(format!("Edited {path}.")),
                        Err(e) => ToolOutcome::err(format!("Could not write `{path}`: {e}.")),
                    }
                }
                n => ToolOutcome::err(format!(
                    "`old_string` appears {n} times in `{path}`. Add surrounding lines so it matches exactly once."
                )),
            }
        })
    }
}

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command (`sh -c`). Default timeout 120s (`timeout_ms` overrides, max 600s); the whole process group is killed on timeout or cancel. Output is stdout+stderr combined, truncated at 50KB."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 120000, max 600000)"}
            },
            "required": ["command"]
        })
    }
    fn permission(&self, input: &Value) -> Permission {
        let cmd = input.get("command").and_then(Value::as_str).unwrap_or("?");
        let short: String = cmd.chars().take(120).collect();
        Permission::Ask { summary: format!("bash [{}]: {short}", sandbox_status().label()) }
    }
    fn run<'a>(&'a self, input: Value, cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let command = match str_arg(&input, "command") {
                Ok(c) => c.to_string(),
                Err(e) => return e,
            };
            let timeout_ms = input
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(BASH_DEFAULT_TIMEOUT_MS)
                .min(BASH_MAX_TIMEOUT_MS);

            let mut cmd = sandbox::build_command(&command, sandbox_status());
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .process_group(0);

            let child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => return ToolOutcome::err(format!("Could not start shell: {e}.")),
            };
            let pid = child.id();
            let wait = child.wait_with_output();
            tokio::pin!(wait);

            let outcome = tokio::select! {
                r = &mut wait => match r {
                    Ok(output) => {
                        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if !stderr.is_empty() {
                            if !text.is_empty() { text.push('\n'); }
                            text.push_str(&stderr);
                        }
                        if text.len() > BASH_MAX_OUTPUT {
                            let cut = text.char_indices().take_while(|(i, _)| *i < BASH_MAX_OUTPUT).count();
                            text.truncate(text.char_indices().nth(cut).map(|(i, _)| i).unwrap_or(BASH_MAX_OUTPUT));
                            text.push_str("\n[output truncated at 50KB — narrow the command (grep/head) to see more]");
                        }
                        if output.status.success() {
                            ToolOutcome::ok(if text.is_empty() { "(no output)".into() } else { text })
                        } else {
                            let code = output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
                            ToolOutcome::err(format!("Command exited with status {code}.\n{text}"))
                        }
                    }
                    Err(e) => ToolOutcome::err(format!("Failed waiting on command: {e}.")),
                },
                _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
                    kill_group(pid);
                    ToolOutcome::err(format!(
                        "Command timed out after {}s and its process group was killed. Re-run with a larger `timeout_ms` or a narrower command.",
                        timeout_ms / 1000
                    ))
                }
                _ = cancel.cancelled() => {
                    kill_group(pid);
                    ToolOutcome::err("Command cancelled by the user.".to_string())
                }
            };
            outcome
        })
    }
}

/// Kill the child's whole process group (it was spawned with process_group(0),
/// so its pgid == its pid).
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: plain syscall; negative pid targets the process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;

    fn run<T: Tool>(tool: &T, input: Value) -> ToolOutcome {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tool.run(input, CancellationToken::new()))
    }

    #[test]
    fn edit_requires_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "aaa\nbbb\naaa\n").unwrap();
        let p = path.to_str().unwrap();

        let dup = run(&EditTool, json!({"path": p, "old_string": "aaa", "new_string": "ccc"}));
        assert!(dup.is_error);
        assert!(dup.content.contains("2 times"));

        let missing = run(&EditTool, json!({"path": p, "old_string": "zzz", "new_string": "ccc"}));
        assert!(missing.is_error && missing.content.contains("not found"));

        let ok = run(&EditTool, json!({"path": p, "old_string": "bbb", "new_string": "BBB"}));
        assert!(!ok.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaa\nBBB\naaa\n");
    }

    #[test]
    fn write_creates_parents_and_read_reports_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.txt");
        let p = path.to_str().unwrap();
        let w = run(&WriteTool, json!({"path": p, "content": "one\ntwo\n"}));
        assert!(!w.is_error, "{}", w.content);
        let r = run(&ReadTool, json!({"path": p}));
        assert!(!r.is_error);
        assert!(r.content.contains("one") && r.content.contains("two"));
    }

    #[test]
    fn bash_captures_exit_and_timeout() {
        let ok = run(&BashTool, json!({"command": "echo hi"}));
        assert!(!ok.is_error);
        assert!(ok.content.contains("hi"));

        let fail = run(&BashTool, json!({"command": "exit 3"}));
        assert!(fail.is_error && fail.content.contains("status 3"));

        let t = run(&BashTool, json!({"command": "sleep 5", "timeout_ms": 200}));
        assert!(t.is_error && t.content.contains("timed out"));
    }
}
