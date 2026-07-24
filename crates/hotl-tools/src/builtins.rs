//! The four M0 built-ins. Failure messages are prompts (they instruct the
//! model); truncation carries continuation hints.

use std::sync::OnceLock;

use crate::sandbox::{self, SandboxStatus};
use crate::{execute_later_reason, Permission, Tool, ToolOutcome};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const READ_MAX_BYTES: usize = 200 * 1024;
const READ_MAX_LINES: usize = 2000;
const BASH_DEFAULT_TIMEOUT_MS: u64 = 120_000;
const BASH_MAX_TIMEOUT_MS: u64 = 600_000;
const BASH_MAX_OUTPUT: usize = 50 * 1024;
/// Slack past the truncation point so `combined_output` still sees "over the
/// cap" and appends its marker exactly as before.
const BASH_OUTPUT_SLACK: usize = 1024;

/// Errors double as results: `Err(ToolOutcome)` is the errors-as-prompts
/// channel, letting tool bodies use `?`.
type ToolResult = Result<ToolOutcome, ToolOutcome>;

fn done(result: ToolResult) -> ToolOutcome {
    result.unwrap_or_else(|e| e)
}

pub(crate) fn sandbox_status() -> &'static SandboxStatus {
    static STATUS: OnceLock<SandboxStatus> = OnceLock::new();
    STATUS.get_or_init(sandbox::probe)
}

fn str_arg<'v>(input: &'v Value, key: &str) -> Result<&'v str, ToolOutcome> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| {
        ToolOutcome::err(format!(
            "Missing required string argument `{key}`. Re-send the call with `{key}` set."
        ))
    })
}

/// Confirm a search root stays inside the working directory. Absolute paths
/// and `..` escapes are refused: `glob`/`grep` are read-only *and*
/// workspace-scoped — that containment is what lets them run without an ask.
/// Pure-lexical (no fs touch), so it can't be defeated by a symlink race.
pub(crate) fn workspace_contained(path: &str) -> Result<std::path::PathBuf, ToolOutcome> {
    let reject = || {
        ToolOutcome::err(format!(
            "`{path}` is outside the working directory. `glob`/`grep` only search the \
             current project; use a relative path inside it."
        ))
    };
    if path.starts_with('/') {
        return Err(reject());
    }
    let mut out: Vec<&str> = Vec::new();
    for part in path.trim_start_matches("./").split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(&seg) if seg != "..") {
                    out.pop();
                } else {
                    return Err(reject()); // escapes above the root
                }
            }
            seg => out.push(seg),
        }
    }
    Ok(std::path::PathBuf::from(if out.is_empty() {
        ".".to_string()
    } else {
        out.join("/")
    }))
}

/// Permission for a mutating file tool: protected paths escalate.
fn file_permission(verb: &str, input: &Value) -> Permission {
    let path = input.get("path").and_then(Value::as_str).unwrap_or("?");
    let summary = format!("{verb} {path}");
    match execute_later_reason(path) {
        Some(why) => Permission::AskProtected {
            summary,
            why: why.into(),
        },
        None => Permission::Ask { summary },
    }
}

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
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
        Box::pin(async move { done(read_impl(&input).await) })
    }
}

async fn read_impl(input: &Value) -> ToolResult {
    use tokio::io::AsyncBufReadExt;
    let path = str_arg(input, "path")?;
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let read_err = |e: std::io::Error| {
        ToolOutcome::err(format!(
            "Could not read `{path}`: {e}. Check the path (use `bash` with `ls` to explore) and try again."
        ))
    };
    // Stream line by line: nothing before `offset` or past the caps is ever
    // retained, but lines are still counted to the end for honest totals.
    let file = tokio::fs::File::open(path).await.map_err(read_err)?;
    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut out = String::new();
    let mut taken = 0usize;
    let mut total = 0usize;
    // 0-based index of the first line the caps excluded.
    let mut truncated_at: Option<usize> = None;
    while let Some(line) = lines.next_line().await.map_err(read_err)? {
        let i = total;
        total += 1;
        if i + 1 < offset || truncated_at.is_some() {
            continue;
        }
        if taken >= READ_MAX_LINES || out.len() + line.len() > READ_MAX_BYTES {
            truncated_at = Some(i);
            continue;
        }
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
        taken += 1;
    }
    if offset > total && total > 0 {
        return Err(ToolOutcome::err(format!(
            "`{path}` has only {total} lines; offset {offset} is past the end."
        )));
    }
    if let Some(i) = truncated_at {
        out.push_str(&format!(
            "\n[truncated: showing lines {offset}-{i} of {total}; continue with offset={}]",
            i + 1
        ));
    }
    if out.is_empty() {
        out = "[empty file]".into();
    }
    Ok(ToolOutcome::ok(out))
}

#[derive(Default)]
pub struct WriteTool {
    pub diag: std::sync::Arc<crate::diagnostics::Diagnostics>,
}

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &str {
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
        file_permission("write", input)
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(
            async move { with_diagnostics(&self.diag, &input, write_impl(&input).await).await },
        )
    }
}

async fn write_impl(input: &Value) -> ToolResult {
    let path = str_arg(input, "path")?;
    let content = str_arg(input, "content")?;
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolOutcome::err(format!(
                    "Could not create parent directories for `{path}`: {e}."
                ))
            })?;
        }
    }
    tokio::fs::write(path, content)
        .await
        .map_err(|e| ToolOutcome::err(format!("Could not write `{path}`: {e}.")))?;
    Ok(ToolOutcome::ok(format!(
        "Wrote {} bytes to {path}.",
        content.len()
    )))
}

#[derive(Default)]
pub struct EditTool {
    pub diag: std::sync::Arc<crate::diagnostics::Diagnostics>,
}

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &str {
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
        file_permission("edit", input)
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move { with_diagnostics(&self.diag, &input, edit_impl(&input).await).await })
    }
}

/// Append the configured post-mutation check (M3a) to a successful result.
async fn with_diagnostics(
    diag: &crate::diagnostics::Diagnostics,
    input: &Value,
    result: ToolResult,
) -> ToolOutcome {
    let mut outcome = done(result);
    if !outcome.is_error {
        if let Ok(path) = str_arg(input, "path") {
            if let Some(report) = diag.check(path).await {
                outcome.content.push_str(&report);
            }
        }
    }
    outcome
}

async fn edit_impl(input: &Value) -> ToolResult {
    let path = str_arg(input, "path")?;
    let old = str_arg(input, "old_string")?;
    let new = str_arg(input, "new_string")?;
    if old.is_empty() {
        return Err(ToolOutcome::err(
            "`old_string` is empty. Use `write` to create a file, or provide the exact text to replace.",
        ));
    }
    let content = tokio::fs::read_to_string(path).await.map_err(|e| {
        ToolOutcome::err(format!(
            "Could not read `{path}`: {e}. Read the file first to confirm the path."
        ))
    })?;
    match crate::matcher::find(&content, old) {
        crate::matcher::Match::None => Err(ToolOutcome::err(format!(
            "`old_string` was not found in `{path}` (even with whitespace-tolerant matching). \
             Read the file and copy the exact text."
        ))),
        crate::matcher::Match::Ambiguous(n) => Err(ToolOutcome::err(format!(
            "`old_string` matches {n} places in `{path}`. Add surrounding lines so it matches exactly once."
        ))),
        crate::matcher::Match::Unique { start, end, exact } => {
            let updated = format!("{}{new}{}", &content[..start], &content[end..]);
            tokio::fs::write(path, updated)
                .await
                .map_err(|e| ToolOutcome::err(format!("Could not write `{path}`: {e}.")))?;
            let note = if exact { "" } else { " (whitespace-tolerant match)" };
            Ok(ToolOutcome::ok(format!("Edited {path}.{note}")))
        }
    }
}

// TODO(task 4): drop these allows once `GlobTool` is re-exported from `lib.rs`
// and registered in `Registry::builtin_with` — until then it's unreachable
// outside `#[cfg(test)]`.
#[allow(dead_code)]
const GLOB_MAX_RESULTS: usize = 1000;

#[allow(dead_code)]
pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "List files in the working directory matching a filename pattern, newest-first is NOT \
         guaranteed — results are sorted by path. Patterns: `*.rs` (suffix), `**/*.rs` (same, \
         recursion is always on), or a bare substring matched against the relative path. Hidden \
         directories (.git, node_modules) are skipped. Returns at most 1000 paths."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "e.g. \"*.rs\", \"**/*.toml\", or a path substring"},
                "path": {"type": "string", "description": "Directory to search under (relative to the working directory; defaults to \".\")"}
            },
            "required": ["pattern"]
        })
    }
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move { done(glob_impl(&input)) })
    }
}

#[allow(dead_code)]
fn glob_impl(input: &Value) -> ToolResult {
    let pattern = str_arg(input, "pattern")?;
    let root = workspace_contained(input.get("path").and_then(Value::as_str).unwrap_or("."))?;
    // "*.rs" / "**/*.rs" -> suffix match on the file name (the substring after
    // the final `*`); a pattern with no `*` -> substring match on the relative
    // path.
    let suffix = pattern
        .contains('*')
        .then(|| pattern.rsplit('*').next().unwrap_or(""));
    let mut hits: Vec<String> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue; // skip vcs/vendor/build dirs
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let matched = match suffix {
                Some(sfx) => name.ends_with(sfx), // "*.rs" -> ".rs"
                None => rel.contains(pattern),    // bare substring
            };
            if matched {
                hits.push(rel);
            }
        }
    }
    hits.sort();
    let total = hits.len();
    let mut out = if total == 0 {
        format!("No files match `{pattern}`. Try a broader pattern, or `grep` to search contents.")
    } else {
        let shown = total.min(GLOB_MAX_RESULTS);
        let mut s = hits[..shown].join("\n");
        if total > shown {
            s.push_str(&format!(
                "\n[truncated: showing {shown} of {total}; narrow the pattern or `path`]"
            ));
        }
        s
    };
    out.push('\n');
    Ok(ToolOutcome::ok(out))
}

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &str {
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
        // The egress marker joins the label only when a policy is configured
        // (`net:off`, `net:allow(N)`, or a loud NET:UNENFORCED); with the
        // default Open policy the label is unchanged.
        let label = match crate::net::label_suffix() {
            Some(net) => format!("{} {net}", sandbox_status().label()),
            None => sandbox_status().label(),
        };
        Permission::Ask {
            summary: format!("bash [{label}]: {short}"),
        }
    }
    fn run<'a>(&'a self, input: Value, cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move { done(bash_impl(&input, cancel).await) })
    }
}

async fn bash_impl(input: &Value, cancel: CancellationToken) -> ToolResult {
    let command = str_arg(input, "command")?;
    let timeout_ms = input
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(BASH_DEFAULT_TIMEOUT_MS)
        .min(BASH_MAX_TIMEOUT_MS);

    let egress = crate::net::egress_state().await;
    let mut cmd = sandbox::build_command(command, sandbox_status(), &egress);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = cmd
        .spawn()
        .map_err(|e| ToolOutcome::err(format!("Could not start shell: {e}.")))?;
    let pid = child.id();
    let wait = collect_output(child, BASH_MAX_OUTPUT + BASH_OUTPUT_SLACK);
    tokio::pin!(wait);

    tokio::select! {
        result = &mut wait => Ok(shell_outcome(result)),
        _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
            kill_group(pid);
            Err(ToolOutcome::err(format!(
                "Command timed out after {}s and its process group was killed. Re-run with a larger `timeout_ms` or a narrower command.",
                timeout_ms / 1000
            )))
        }
        _ = cancel.cancelled() => {
            kill_group(pid);
            Err(ToolOutcome::err("Command cancelled by the user."))
        }
    }
}

/// Incrementally read the child's stdout/stderr (capped at `cap` bytes each)
/// and then wait for its exit status. Unlike `wait_with_output`, this never
/// buffers unbounded output: past the cap the pipes are still drained (and
/// discarded) so the child can't block on a full pipe. Shared with the
/// diagnostics runner (H-11).
pub(crate) async fn collect_output(
    mut child: tokio::process::Child,
    cap: usize,
) -> std::io::Result<std::process::Output> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (stdout, stderr) = tokio::join!(drain_capped(stdout, cap), drain_capped(stderr, cap));
    let status = child.wait().await?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Read a pipe to EOF in 8KB chunks, keeping at most `cap` bytes.
async fn drain_capped<R: tokio::io::AsyncRead + Unpin>(reader: Option<R>, cap: usize) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let Some(mut reader) = reader else { return buf };
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    buf
}

fn shell_outcome(result: std::io::Result<std::process::Output>) -> ToolOutcome {
    let output = match result {
        Ok(o) => o,
        Err(e) => return ToolOutcome::err(format!("Failed waiting on command: {e}.")),
    };
    let text = combined_output(&output);
    if output.status.success() {
        ToolOutcome::ok(if text.is_empty() {
            "(no output)".to_string()
        } else {
            text
        })
    } else {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        ToolOutcome::err(format!("Command exited with status {code}.\n{text}"))
    }
}

fn combined_output(output: &std::process::Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    if text.len() > BASH_MAX_OUTPUT {
        let mut end = BASH_MAX_OUTPUT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[output truncated at 50KB — narrow the command (grep/head) to see more]");
    }
    text
}

/// Kill the child's whole process group (spawned with process_group(0),
/// so its pgid == its pid). Shared with the diagnostics runner (H-11).
pub(crate) fn kill_group(pid: Option<u32>) {
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
    fn workspace_contained_rejects_escape_and_absolute() {
        // relative, in-workspace: allowed (returned path is the cleaned relative)
        assert!(workspace_contained("src").is_ok());
        assert!(workspace_contained("./src/lib.rs").is_ok());
        assert!(workspace_contained(".").is_ok());
        // absolute path: refused (a read tool is not an exfiltration primitive)
        assert!(workspace_contained("/etc/passwd").is_err());
        // traversal out of the workspace: refused
        assert!(workspace_contained("../secrets").is_err());
        assert!(workspace_contained("src/../../etc").is_err());
        // a `..` that stays inside is fine
        assert!(workspace_contained("src/../README.md").is_ok());
    }

    #[tokio::test]
    async fn glob_matches_by_suffix_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "x").unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "x").unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "x").unwrap();

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let out = GlobTool
            .run(json!({"pattern": "*.rs"}), CancellationToken::new())
            .await;
        std::env::set_current_dir(prev).unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/a.rs") && out.content.contains("src/b.rs"));
        assert!(!out.content.contains("README.md"), "suffix filter failed");
        assert!(
            !out.content.contains(".git/"),
            "hidden dirs must be skipped"
        );
    }

    #[tokio::test]
    async fn glob_refuses_escape() {
        let out = GlobTool
            .run(
                json!({"pattern": "*.rs", "path": "/etc"}),
                CancellationToken::new(),
            )
            .await;
        assert!(out.is_error && out.content.contains("outside the working directory"));
    }

    #[test]
    fn edit_requires_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "aaa\nbbb\naaa\n").unwrap();
        let p = path.to_str().unwrap();

        let dup = run(
            &EditTool::default(),
            json!({"path": p, "old_string": "aaa", "new_string": "ccc"}),
        );
        assert!(dup.is_error);
        assert!(dup.content.contains("matches 2 places"));

        let missing = run(
            &EditTool::default(),
            json!({"path": p, "old_string": "zzz", "new_string": "ccc"}),
        );
        assert!(missing.is_error && missing.content.contains("not found"));

        let ok = run(
            &EditTool::default(),
            json!({"path": p, "old_string": "bbb", "new_string": "BBB"}),
        );
        assert!(!ok.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaa\nBBB\naaa\n");
    }

    #[test]
    fn write_creates_parents_and_read_reports_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.txt");
        let p = path.to_str().unwrap();
        let w = run(
            &WriteTool::default(),
            json!({"path": p, "content": "one\ntwo\n"}),
        );
        assert!(!w.is_error, "{}", w.content);
        let r = run(&ReadTool, json!({"path": p}));
        assert!(!r.is_error);
        assert!(r.content.contains("one") && r.content.contains("two"));
    }

    #[test]
    fn only_read_is_parallel_safe_among_builtins() {
        // read has no side effects, so calls in one batch may overlap; the
        // mutating/executing builtins must stay serial within a batch.
        assert!(ReadTool.parallel_safe());
        assert!(!EditTool::default().parallel_safe());
        assert!(!WriteTool::default().parallel_safe());
        assert!(!BashTool.parallel_safe());
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
