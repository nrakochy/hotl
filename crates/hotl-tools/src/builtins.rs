//! The four M0 built-ins. Failure messages are prompts (they instruct the
//! model); truncation carries continuation hints.

use std::sync::OnceLock;

use crate::sandbox::{self, SandboxStatus};
use crate::{execute_later_reason, fsguard, Permission, Tool, ToolOutcome};
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

/// Resolve a search tool's `path` argument into a walk root.
///
/// INVARIANT: containment is decided on the fd, not the name. The lexical
/// `classify` only picks the error message; `resolve_beneath` is the boundary
/// and refuses a search root reached through a symlink. Enforced by
/// `glob_refuses_a_symlinked_search_root` and
/// `grep_refuses_a_symlinked_path_argument`.
fn guarded_search_root(
    tool: &str,
    root: &std::path::Path,
    given: &str,
    dir_only: bool,
) -> Result<std::path::PathBuf, ToolOutcome> {
    let rel = match fsguard::classify(root, given) {
        fsguard::Placement::Inside(rel) => rel,
        fsguard::Placement::Outside(_) => {
            return Err(ToolOutcome::err(format!(
                "`{given}` is outside the working directory. `{tool}` only searches the current \
                 project; use `read` with an absolute path (it will ask) for anything else."
            )))
        }
    };
    // `glob` walks its root, so the root has to be a directory; `grep` is
    // happy with a single file.
    if dir_only {
        fsguard::open_dir_beneath(root, &rel).map_err(|e| match e {
            fsguard::GuardError::Io(ref io) if io.raw_os_error() == Some(libc::ENOTDIR) => {
                ToolOutcome::err(format!(
                    "`{given}` is not a directory. `{tool}` lists files *under* a directory — \
                     pass the containing directory, or use `grep` to search one file."
                ))
            }
            other => ToolOutcome::err(other.prompt(tool, given)),
        })?;
    }
    fsguard::resolve_beneath(root, &rel).map_err(|e| ToolOutcome::err(e.prompt(tool, given)))
}

/// Permission for a mutating file tool: protected paths escalate.
///
/// INVARIANT: the execute-later classification runs on the *resolved* target
/// as well as the literal path, so an innocent-looking name that is a symlink
/// to `~/.zshrc` still gets the escalated ask — a symlink cannot launder a
/// protected write into an ordinary one. Enforced by
/// `file_permission_classifies_the_resolved_target`.
fn file_permission(verb: &str, input: &Value) -> Permission {
    let path = input.get("path").and_then(Value::as_str).unwrap_or("?");
    let summary = format!("{verb} {path}");
    let resolved = std::fs::canonicalize(path).ok();
    let reason = execute_later_reason(path).or_else(|| {
        resolved
            .as_deref()
            .and_then(|r| execute_later_reason(&r.to_string_lossy()))
    });
    let outside = matches!(
        fsguard::classify(fsguard::workspace_root(), path),
        fsguard::Placement::Outside(_)
    );
    match (reason, outside) {
        (Some(why), _) => Permission::AskProtected {
            summary,
            why: why.into(),
        },
        (None, true) => Permission::AskProtected {
            summary,
            why: "outside the working directory: not covered by the sandbox write-confinement \
                  floor"
                .into(),
        },
        (None, false) => Permission::Ask { summary },
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
    fn read_only(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Read a text file. Paths are relative to the working directory; a path outside it (or one \
         that leaves it through a symlink) is allowed but requires explicit approval. Returns at \
         most 2000 lines / 200KB per call; use `offset` (1-indexed start line) to continue a \
         truncated read."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the working directory. An absolute path outside the working directory is permitted but prompts for approval."},
                "offset": {"type": "integer", "description": "1-indexed line to start from (for continuing truncated reads)"}
            },
            "required": ["path"]
        })
    }
    /// INVARIANT: a read that stays inside the workspace runs unprompted; a
    /// read that leaves it is `AskProtected` — the one tier that outranks
    /// `mode=auto` (`rules.rs:232` vs `rules.rs:253`) — so the boundary is
    /// real in the shipped default configuration, not just in `ask` mode.
    /// Enforced by `read_outside_the_workspace_is_protected_not_free`.
    fn permission(&self, input: &Value) -> Permission {
        let path = input.get("path").and_then(Value::as_str).unwrap_or("?");
        match fsguard::classify(fsguard::workspace_root(), path) {
            fsguard::Placement::Inside(_) => Permission::None,
            fsguard::Placement::Outside(_) => Permission::AskProtected {
                summary: format!("read {path}"),
                // Show the human where it really lands, links resolved.
                why: format!(
                    "outside the working directory{}: the workspace boundary does not cover it",
                    match std::fs::canonicalize(path) {
                        Ok(real) if real != std::path::Path::new(path) =>
                            format!(" (resolves to {})", real.display()),
                        _ => String::new(),
                    }
                ),
            },
        }
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move { done(read_in(fsguard::workspace_root(), &input).await) })
    }
}

/// `root`-parameterized so tests drive it against a tempdir without touching
/// the process-global cwd (the same reason `glob_walk`/`grep_search` were
/// split out).
///
/// INVARIANT: permission-time classification and run-time resolution must
/// agree, or the call is refused. A path `classify` called `Inside` that turns
/// out to escape through a symlink is refused with a prompt — never silently
/// opened, and never silently upgraded to the protected ask it should have
/// had. That is what makes the fd check safe to do *after* the ask, with no
/// shared state between the two. Enforced by
/// `read_refuses_a_symlink_out_of_the_workspace_and_says_how_to_proceed`.
async fn read_in(root: &std::path::Path, input: &Value) -> ToolResult {
    let path = str_arg(input, "path")?;
    let file = match fsguard::classify(root, path) {
        // Inside: the fd descent is the boundary.
        fsguard::Placement::Inside(rel) => fsguard::open_beneath(root, &rel)
            .map_err(|e| ToolOutcome::err(e.prompt("read", path)))?,
        // Outside: the human approved *this path*, so open it plainly —
        // following links is what they agreed to.
        fsguard::Placement::Outside(_) => std::fs::File::open(path).map_err(|e| {
            ToolOutcome::err(format!(
                "Could not read `{path}`: {e}. Check the path (use `glob`) and try again."
            ))
        })?,
    };
    read_stream(tokio::fs::File::from_std(file), path, input).await
}

async fn read_stream(file: tokio::fs::File, path: &str, input: &Value) -> ToolResult {
    use tokio::io::AsyncBufReadExt;
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
    // retained, but lines are still counted to the end for honest totals. The
    // handle arrives already validated by the guard — this never re-opens by
    // name, which is the whole point of the containment layer.
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
        Box::pin(async move {
            let root = fsguard::workspace_root();
            with_diagnostics(&self.diag, &input, write_in(root, &input).await).await
        })
    }
}

/// INVARIANT: a write inside the workspace goes through the guarded create,
/// so no component of the path — including the final one — is ever a symlink
/// that would redirect the bytes somewhere else. A path outside the workspace
/// was approved explicitly by the human, who saw the resolved target in the
/// ask, so it is written plainly. Enforced by
/// `write_through_a_symlink_does_not_touch_the_target`.
async fn write_in(root: &std::path::Path, input: &Value) -> ToolResult {
    let path = str_arg(input, "path")?;
    let content = str_arg(input, "content")?;
    match fsguard::classify(root, path) {
        fsguard::Placement::Inside(rel) => {
            use std::io::Write;
            let mut f = fsguard::create_beneath(root, &rel, true)
                .map_err(|e| ToolOutcome::err(e.prompt("write", path)))?;
            f.write_all(content.as_bytes())
                .and_then(|()| f.sync_all())
                .map_err(|e| ToolOutcome::err(format!("Could not write `{path}`: {e}.")))?;
        }
        fsguard::Placement::Outside(_) => {
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
        }
    }
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
        Box::pin(async move {
            let root = fsguard::workspace_root();
            with_diagnostics(&self.diag, &input, edit_in(root, &input).await).await
        })
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

/// INVARIANT: both halves of an edit — the read that finds the match and the
/// write that applies it — go through the guard, so an in-workspace name that
/// is a symlink out of the tree is refused rather than silently rewriting the
/// link's target. Enforced by `edit_through_a_symlink_does_not_touch_the_target`.
async fn edit_in(root: &std::path::Path, input: &Value) -> ToolResult {
    let path = str_arg(input, "path")?;
    let old = str_arg(input, "old_string")?;
    let new = str_arg(input, "new_string")?;
    if old.is_empty() {
        return Err(ToolOutcome::err(
            "`old_string` is empty. Use `write` to create a file, or provide the exact text to replace.",
        ));
    }
    let placement = fsguard::classify(root, path);
    let content = read_guarded_to_string(root, &placement, path)?;
    match crate::matcher::find(&content, old) {
        crate::matcher::Match::None => Err(ToolOutcome::err(format!(
            "`old_string` was not found in `{path}` (even with whitespace-tolerant matching). \
             Read the file and copy the exact text."
        ))),
        crate::matcher::Match::Ambiguous(n) => Err(ToolOutcome::err(format!(
            "`old_string` matches {n} places in `{path}`. Add surrounding lines so it matches exactly once."
        ))),
        crate::matcher::Match::Unique {
            start,
            end,
            exact,
            reindent,
        } => {
            // A tolerant match fired *because* the model's whitespace differs
            // from the file's, so splicing `new_string` verbatim would install
            // the model's indentation over the file's.
            let spliced = match &reindent {
                Some(r) => crate::matcher::rebase_indent(new, r),
                None => new.to_string(),
            };
            let updated = format!("{}{spliced}{}", &content[..start], &content[end..]);
            write_guarded(root, &placement, path, updated.as_bytes())?;
            let note = if exact {
                ""
            } else {
                " (whitespace-tolerant match; the file's indentation was preserved)"
            };
            Ok(ToolOutcome::ok(format!("Edited {path}.{note}")))
        }
    }
}

/// Read a file for editing through whichever door its `Placement` opened:
/// the fd descent inside the workspace, a plain open outside it (which the
/// human approved by path).
fn read_guarded_to_string(
    root: &std::path::Path,
    placement: &fsguard::Placement,
    path: &str,
) -> Result<String, ToolOutcome> {
    let not_read = |e: std::io::Error| {
        ToolOutcome::err(format!(
            "Could not read `{path}`: {e}. Read the file first to confirm the path."
        ))
    };
    match placement {
        fsguard::Placement::Inside(rel) => {
            use std::io::Read;
            let mut f = fsguard::open_beneath(root, rel)
                .map_err(|e| ToolOutcome::err(e.prompt("edit", path)))?;
            let mut s = String::new();
            f.read_to_string(&mut s).map_err(not_read)?;
            Ok(s)
        }
        fsguard::Placement::Outside(_) => std::fs::read_to_string(path).map_err(not_read),
    }
}

/// The write-back half of `read_guarded_to_string`. Inside the workspace this
/// is the atomic replace (temp sibling, fsync, `renameat`, parent fsync);
/// outside it, a plain whole-file write.
fn write_guarded(
    root: &std::path::Path,
    placement: &fsguard::Placement,
    path: &str,
    bytes: &[u8],
) -> Result<(), ToolOutcome> {
    match placement {
        fsguard::Placement::Inside(rel) => fsguard::replace_beneath(root, rel, bytes)
            .map_err(|e| ToolOutcome::err(e.prompt("edit", path))),
        fsguard::Placement::Outside(_) => std::fs::write(path, bytes)
            .map_err(|e| ToolOutcome::err(format!("Could not write `{path}`: {e}."))),
    }
}

const GLOB_MAX_RESULTS: usize = 1000;
const GLOB_MAX_DEPTH: usize = 64;
const GLOB_MAX_ENTRIES: usize = 200_000;
const GLOB_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn read_only(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "List files under the working directory matching a glob. Real glob syntax: `*` (does not \
         cross `/`), `**` (recurses), `?`, `[a-z]`, `{a,b}`. A pattern with no `/` matches the \
         file name at any depth (`*.rs`). Results are newest-first by default (`sort`: \"mtime\" \
         | \"path\"), capped at 1000. Respects .gitignore; `.git` is never walked; symlinks are \
         never followed."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob, e.g. \"*.rs\", \"src/**/*.rs\", \"src/*/mod.rs\", \"**/*.{ts,tsx}\""},
                "path": {"type": "string", "description": "Directory to search under (relative to the working directory; defaults to \".\")"},
                "sort": {"type": "string", "enum": ["mtime", "path"], "description": "Result order; defaults to \"mtime\" (newest first)"}
            },
            "required": ["pattern"]
        })
    }
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    fn run<'a>(&'a self, input: Value, cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(
            async move { done(glob_run_input(fsguard::workspace_root(), &input, cancel).await) },
        )
    }
}

/// `root`-parameterized so tests drive the walk against a tempdir without
/// touching the process-global cwd (`std::env::set_current_dir` would race
/// every other test in this crate that spawns a subprocess relying on ambient
/// cwd).
async fn glob_run_input(
    root: &std::path::Path,
    input: &Value,
    cancel: CancellationToken,
) -> ToolResult {
    let pattern = str_arg(input, "pattern")?.to_string();
    let base = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let sort_mtime = input.get("sort").and_then(Value::as_str) != Some("path");
    // The search root goes through the guard: `path: "link"` cannot start the
    // walk outside the tree. After this, the resolved path IS the real path
    // (no component was a link), so handing it to `ignore` is safe.
    let dir = guarded_search_root("glob", root, base, true)?;

    let matcher = globset::GlobBuilder::new(&pattern)
        // `*` stops at `/` only for patterns that speak in paths; a bare
        // `*.rs` is matched against the file name, where there is no `/`.
        .literal_separator(pattern.contains('/'))
        .build()
        .map_err(|e| {
            ToolOutcome::err(format!(
                "`{pattern}` is not a valid glob: {e}. Try `*.rs`, `src/**/*.rs`, or `src/*/mod.rs`."
            ))
        })?
        .compile_matcher();
    let name_only = !pattern.contains('/');

    // INVARIANT: the walk runs on the blocking pool, never on a tokio worker —
    // a large or hostile tree must not stall the runtime. Enforced by
    // `glob_terminates_on_a_symlink_loop_and_never_leaves_the_tree`.
    let walk_root = dir.clone();
    let c = cancel.clone();
    let handle = tokio::task::spawn_blocking(move || -> Result<(Vec<GlobHit>, bool), ()> {
        let deadline = std::time::Instant::now() + GLOB_DEADLINE;
        let mut hits: Vec<GlobHit> = Vec::new();
        let mut seen = 0usize;
        let mut bounded = false;
        let walker = ignore::WalkBuilder::new(&walk_root)
            .follow_links(false) // containment layer 1 — and `ignore`'s default
            .max_depth(Some(GLOB_MAX_DEPTH))
            .hidden(false) // `.github/workflows/*.yml` must be reachable
            .filter_entry(|e| e.file_name() != ".git")
            .build();
        for entry in walker {
            seen += 1;
            // `spawn_blocking` tasks cannot be aborted from outside, so the
            // token is polled *here*; the caller's `select!` only stops
            // waiting. Both halves are needed.
            if c.is_cancelled() {
                return Err(());
            }
            if seen > GLOB_MAX_ENTRIES || std::time::Instant::now() > deadline {
                bounded = true;
                break; // report what we have with a truncation hint
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&walk_root)
                .unwrap_or(entry.path())
                .to_path_buf();
            let subject: &std::ffi::OsStr = if name_only {
                entry.file_name()
            } else {
                rel.as_os_str()
            };
            if matcher.is_match(std::path::Path::new(subject)) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::UNIX_EPOCH);
                hits.push(GlobHit { rel, mtime });
            }
        }
        Ok((hits, bounded))
    });

    let cancelled = || ToolOutcome::err("Search cancelled by the user.");
    let (mut hits, bounded) = tokio::select! {
        joined = handle => joined
            .map_err(|e| ToolOutcome::err(format!(
                "The file walk failed: {e}. Try a narrower `path`."
            )))?
            .map_err(|()| cancelled())?,
        () = cancel.cancelled() => return Err(cancelled()),
    };
    if sort_mtime {
        hits.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.rel.cmp(&b.rel)));
    } else {
        hits.sort_by(|a, b| a.rel.cmp(&b.rel));
    }

    let total = hits.len();
    let mut out = if total == 0 {
        format!("No files match `{pattern}`. Try a broader pattern, or `grep` to search contents.")
    } else {
        let shown = total.min(GLOB_MAX_RESULTS);
        let mut s = hits[..shown]
            .iter()
            .map(|h| h.rel.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        if total > shown {
            s.push_str(&format!(
                "\n[truncated: showing {shown} of {total}; narrow the pattern or `path`]"
            ));
        }
        s
    };
    if bounded {
        out.push_str(
            "\n[the walk hit its entry/time bound before finishing; narrow `path` to see the rest]",
        );
    }
    out.push('\n');
    Ok(ToolOutcome::ok(out))
}

struct GlobHit {
    rel: std::path::PathBuf,
    mtime: std::time::SystemTime,
}

const GREP_MAX_OUTPUT: usize = 50 * 1024;

pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn read_only(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Search file contents in the working directory with ripgrep. `pattern` is a regular \
         expression. Optional `path` (relative), `glob` (e.g. \"*.rs\" to filter files), and \
         `files_only` (list matching file names only). Output is capped at 50KB; narrow the \
         pattern or add `glob` if it truncates."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for"},
                "path": {"type": "string", "description": "Directory or file to search (relative to the working directory; defaults to \".\")"},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. \"*.rs\""},
                "files_only": {"type": "boolean", "description": "List matching file paths instead of matching lines"}
            },
            "required": ["pattern"]
        })
    }
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    fn run<'a>(&'a self, input: Value, cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move { done(grep_in(fsguard::workspace_root(), &input, cancel).await) })
    }
}

/// INVARIANT: ripgrep does not follow links while walking, but it DOES follow
/// a link passed as an explicit path argument — so the argument is resolved
/// through the guard before it becomes argv. Enforced by
/// `grep_refuses_a_symlinked_path_argument` (the walk half, which ripgrep
/// already gets right, is pinned by `grep_does_not_follow_links_inside_the_tree`).
async fn grep_in(root: &std::path::Path, input: &Value, cancel: CancellationToken) -> ToolResult {
    let pattern = str_arg(input, "pattern")?;
    let given = input.get("path").and_then(Value::as_str).unwrap_or(".");
    let target = guarded_search_root("grep", root, given, false)?;
    grep_search(pattern, &target, input, cancel).await
}

/// The ripgrep invocation itself, taking an already-resolved root (relative-
/// and-contained at the call site above; an absolute tempdir path works just
/// as well — it's just another argv element to `rg`). Split out for the same
/// reason as `glob_walk`: tests drive this directly against a tempdir instead
/// of mutating the process-global working directory.
async fn grep_search(
    pattern: &str,
    root: &std::path::Path,
    input: &Value,
    cancel: CancellationToken,
) -> ToolResult {
    // Fixed argv — the model's pattern is a value, never spliced into a shell.
    let mut args = vec![
        "--line-number".to_string(),
        "--no-heading".to_string(),
        "--color=never".to_string(),
        "--max-columns=400".to_string(),
    ];
    if input
        .get("files_only")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        args.push("--files-with-matches".to_string());
    }
    if let Some(g) = input.get("glob").and_then(Value::as_str) {
        args.push("--glob".to_string());
        args.push(g.to_string());
    }
    args.push("--".to_string());
    args.push(pattern.to_string());
    args.push(root.to_string_lossy().to_string());

    // Reuse the sandbox command builder so content search inherits the floor,
    // but exec `rg` directly (no `sh -c`) so the pattern can never be spliced
    // into a shell string.
    let egress = crate::net::egress_state().await;
    let mut cmd = sandbox::build_argv("rg", &args, sandbox_status(), &egress);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = cmd.spawn().map_err(|e| {
        ToolOutcome::err(format!(
            "Could not run ripgrep: {e}. Is `rg` installed? Fall back to `bash` with grep/find."
        ))
    })?;
    let pid = child.id();
    let wait = collect_output(child, GREP_MAX_OUTPUT + 1024);
    tokio::pin!(wait);
    let output = tokio::select! {
        r = &mut wait => r.map_err(|e| ToolOutcome::err(format!("ripgrep failed: {e}.")))?,
        _ = cancel.cancelled() => {
            kill_group(pid);
            return Err(ToolOutcome::err("Search cancelled by the user."));
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    // rg exit 1 == no matches (success, a prompt); >1 == real error.
    match output.status.code() {
        Some(0) => {
            let mut body = text.to_string();
            if body.len() > GREP_MAX_OUTPUT {
                let mut end = GREP_MAX_OUTPUT;
                while !body.is_char_boundary(end) {
                    end -= 1;
                }
                body.truncate(end);
                body.push_str("\n[truncated at 50KB: narrow `pattern` or add a `glob` filter]");
            }
            Ok(ToolOutcome::ok(body))
        }
        Some(1) => Ok(ToolOutcome::ok(format!(
            "No matches for `{pattern}`. Try a looser pattern or a different `path`/`glob`."
        ))),
        _ => Err(ToolOutcome::err(format!(
            "ripgrep error: {}. Check the pattern (it is a regex).",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
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
    fn read_inside_the_workspace_needs_no_permission() {
        let inside = json!({"path": "Cargo.toml"});
        assert_eq!(ReadTool.permission(&inside), Permission::None);
        assert_eq!(
            ReadTool.permission(&json!({"path": "src/../src/lib.rs"})),
            Permission::None
        );
    }

    #[test]
    fn read_outside_the_workspace_is_protected_not_free() {
        // T1-6's headline: this ran with NO prompt, in every mode, including
        // plan and dontask.
        let out = ReadTool.permission(&json!({"path": "/Users/you/.ssh/id_rsa"}));
        match out {
            Permission::AskProtected { summary, why } => {
                assert!(summary.contains("id_rsa"));
                assert!(why.contains("outside the working directory"), "{why}");
            }
            other => panic!("out-of-workspace read must be protected, got {other:?}"),
        }
        // `..` escapes are the same class.
        assert!(matches!(
            ReadTool.permission(&json!({"path": "../../etc/shadow"})),
            Permission::AskProtected { .. }
        ));
    }

    #[tokio::test]
    async fn read_refuses_a_symlink_out_of_the_workspace_and_says_how_to_proceed() {
        let (_o, root, home) = fsguard::tests::fixture();
        std::os::unix::fs::symlink(home.join("id_rsa"), root.join("notes.md")).unwrap();
        let out = done(read_in(&root, &json!({"path": "notes.md"})).await);
        assert!(out.is_error);
        assert!(
            !out.content.contains("BEGIN PRIVATE KEY"),
            "leaked through the link"
        );
        assert!(
            out.content.contains("absolute path"),
            "must be a prompt: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn an_approved_absolute_read_still_works() {
        // The escalation is an ask, not a ban: reading ~/.gitconfig is a
        // legitimate request that should simply be prompted.
        let (_o, _root, home) = fsguard::tests::fixture();
        let p = home.join("id_rsa");
        let out = done(
            read_in(
                fsguard::workspace_root(),
                &json!({"path": p.to_str().unwrap()}),
            )
            .await,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("BEGIN PRIVATE KEY"));
    }

    #[tokio::test]
    async fn write_through_a_symlink_does_not_touch_the_target() {
        let (_o, root, home) = fsguard::tests::fixture();
        let target = home.join(".zshrc");
        std::fs::write(&target, "# original\n").unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::os::unix::fs::symlink(&target, root.join("docs/notes.md")).unwrap();

        let out = done(write_in(&root, &json!({"path": "docs/notes.md", "content": "evil"})).await);
        assert!(out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "# original\n");
        assert!(out.content.contains("symlink"), "{}", out.content);
    }

    #[test]
    fn file_permission_classifies_the_resolved_target() {
        // The laundering case: an innocent name pointing at a protected file must
        // get the escalated ask, not the ordinary one.
        let (_o, root, home) = fsguard::tests::fixture();
        let rc = home.join(".zshrc");
        std::fs::write(&rc, "").unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::os::unix::fs::symlink(&rc, root.join("docs/notes.md")).unwrap();
        let p = root.join("docs/notes.md");
        match file_permission("write", &json!({"path": p.to_str().unwrap()})) {
            Permission::AskProtected { why, .. } => {
                assert!(why.contains("shell startup file"), "{why}")
            }
            other => panic!("symlink to .zshrc must escalate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_creates_parents_but_never_through_a_link() {
        let (_o, root, home) = fsguard::tests::fixture();
        std::os::unix::fs::symlink(&home, root.join("out")).unwrap();
        let out = done(write_in(&root, &json!({"path": "out/a/b.txt", "content": "x"})).await);
        assert!(out.is_error, "{}", out.content);
        assert!(!home.join("a/b.txt").exists(), "created through the link");
        // The ordinary case still works.
        let ok = done(write_in(&root, &json!({"path": "a/b/c.txt", "content": "x"})).await);
        assert!(!ok.is_error, "{}", ok.content);
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/c.txt")).unwrap(),
            "x"
        );
    }

    #[tokio::test]
    async fn edit_through_a_symlink_does_not_touch_the_target() {
        let (_o, root, home) = fsguard::tests::fixture();
        let target = home.join(".zshrc");
        std::fs::write(&target, "export PATH=/usr/bin\n").unwrap();
        std::os::unix::fs::symlink(&target, root.join("notes.md")).unwrap();
        let out = done(
            edit_in(
                &root,
                &json!({"path": "notes.md", "old_string": "/usr/bin", "new_string": "/evil"}),
            )
            .await,
        );
        assert!(out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "export PATH=/usr/bin\n"
        );
    }

    #[tokio::test]
    async fn edit_keeps_the_files_indentation_on_a_tolerant_match() {
        let (_o, root, _home) = fsguard::tests::fixture();
        // File uses tabs; the model reproduces the block with spaces.
        std::fs::write(root.join("f.rs"), "fn f() {\n\tif x {\n\t\ta();\n\t}\n}\n").unwrap();
        let ok = done(
            edit_in(
                &root,
                &json!({
                    "path": "f.rs",
                    "old_string": "    if x {\n        a();\n    }",
                    "new_string": "    if x {\n        b();\n    }",
                }),
            )
            .await,
        );
        assert!(!ok.is_error, "{}", ok.content);
        // The file's tabs survive; only the content changed.
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "fn f() {\n\tif x {\n\t\tb();\n\t}\n}\n",
            "the model's indentation must not overwrite the file's"
        );
        assert!(
            ok.content.contains("indentation was preserved"),
            "{}",
            ok.content
        );
    }

    #[tokio::test]
    async fn edit_inside_the_tree_still_works_and_keeps_the_mode() {
        let (_o, root, _home) = fsguard::tests::fixture();
        let script = root.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\necho old\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let out = done(
            edit_in(
                &root,
                &json!({"path": "run.sh", "old_string": "echo old", "new_string": "echo new"}),
            )
            .await,
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            "#!/bin/sh\necho new\n"
        );
        // An atomic replace must not silently drop the executable bit.
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "mode {mode:o}");
        // ...and it must leave no temp sibling behind.
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".hotl-tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Drives the walk against an absolute tempdir path rather than
    /// `set_current_dir`: the process-global cwd is shared with every other
    /// test in this crate (several spawn subprocesses that rely on ambient
    /// cwd, e.g. `sandbox::tests::seatbelt_confines_writes`), so flipping it
    /// here would race them under the default parallel test harness.
    async fn glob_run(
        root: &std::path::Path,
        pattern: &str,
        base: &str,
        cancel: CancellationToken,
    ) -> ToolResult {
        glob_run_input(root, &json!({"pattern": pattern, "path": base}), cancel).await
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

        let out = done(glob_run(dir.path(), "*.rs", ".", CancellationToken::new()).await);

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/a.rs") && out.content.contains("src/b.rs"));
        assert!(!out.content.contains("README.md"), "suffix filter failed");
        assert!(!out.content.contains(".git/"), "`.git` is never walked");
    }

    #[tokio::test]
    async fn glob_handles_real_glob_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for p in [
            "src/a/mod.rs",
            "src/b/mod.rs",
            "src/top.rs",
            "docs/x.md",
            ".github/workflows/ci.yml",
        ] {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }

        // T2-10b: this silently matched NOTHING before — `src/*/mod.rs` became
        // the suffix `/mod.rs`, and a bare file name never contains `/`.
        let m = done(glob_run(root, "src/*/mod.rs", ".", CancellationToken::new()).await);
        assert!(
            m.content.contains("src/a/mod.rs") && m.content.contains("src/b/mod.rs"),
            "{}",
            m.content
        );
        assert!(!m.content.contains("src/top.rs"), "`*` must not cross `/`");
        // `**` recurses; `src/` is honored (it was ignored before).
        let r = done(glob_run(root, "src/**/*.rs", ".", CancellationToken::new()).await);
        assert!(
            r.content.contains("src/a/mod.rs") && r.content.contains("src/top.rs"),
            "{}",
            r.content
        );
        assert!(!r.content.contains("docs/"), "{}", r.content);
        // A bare pattern with no `/` matches the file name at any depth.
        let md = done(glob_run(root, "*.md", ".", CancellationToken::new()).await);
        assert!(md.content.contains("docs/x.md"), "{}", md.content);
        // Hidden directories are reachable — this was structurally impossible.
        let ci = done(
            glob_run(
                root,
                ".github/workflows/*.yml",
                ".",
                CancellationToken::new(),
            )
            .await,
        );
        assert!(ci.content.contains("ci.yml"), "{}", ci.content);
        // A malformed pattern is a prompt, not a panic.
        let bad = done(glob_run(root, "src/[", ".", CancellationToken::new()).await);
        assert!(
            bad.is_error && bad.content.contains("not a valid glob"),
            "{}",
            bad.content
        );
    }

    #[tokio::test]
    async fn glob_terminates_on_a_symlink_loop_and_never_leaves_the_tree() {
        let (_o, root, home) = fsguard::tests::fixture();
        std::os::unix::fs::symlink(&root, root.join("src/loop")).unwrap(); // self-loop
        std::os::unix::fs::symlink(&home, root.join("escape")).unwrap(); // escape
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            glob_run(&root, "**/*", ".", CancellationToken::new()),
        )
        .await
        .expect("symlink loop must not hang the walk");
        let out = done(out);
        assert!(
            !out.content.contains("id_rsa"),
            "walked out through a symlink"
        );
    }

    #[tokio::test]
    async fn glob_refuses_a_symlinked_search_root() {
        let (_o, root, home) = fsguard::tests::fixture();
        std::os::unix::fs::symlink(&home, root.join("link")).unwrap();
        let out = done(glob_run(&root, "*", "link", CancellationToken::new()).await);
        assert!(
            out.is_error && out.content.contains("symlink"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn glob_honors_cancellation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (_o, root, _h) = fsguard::tests::fixture();
        let out = done(glob_run(&root, "**/*", ".", cancel).await);
        assert!(
            out.is_error && out.content.to_lowercase().contains("cancel"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn glob_sorts_newest_first_by_default() {
        // Claude Code's Glob orders by mtime; exploration wants recent files.
        // Names are chosen so path order and mtime order disagree.
        let (_o, root, _h) = fsguard::tests::fixture();
        for (name, secs) in [("a_old.rs", 1_000_000u64), ("b_new.rs", 2_000_000)] {
            let p = root.join(name);
            std::fs::write(&p, "x").unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(t))
                .unwrap();
        }
        let by_mtime = done(glob_run(&root, "*.rs", ".", CancellationToken::new()).await);
        assert!(
            by_mtime.content.find("b_new.rs") < by_mtime.content.find("a_old.rs"),
            "newest first: {}",
            by_mtime.content
        );
        let by_path = done(
            glob_run_input(
                &root,
                &json!({"pattern": "*.rs", "sort": "path"}),
                CancellationToken::new(),
            )
            .await,
        );
        assert!(
            by_path.content.find("a_old.rs") < by_path.content.find("b_new.rs"),
            "`sort: path` must be lexicographic: {}",
            by_path.content
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

    #[tokio::test]
    async fn grep_finds_matches_and_reports_no_matches_cleanly() {
        // Drives `grep_search` directly against an absolute tempdir path —
        // same rationale as `glob_matches_by_suffix_and_caps` above: no
        // `set_current_dir`, no race with the rest of the suite.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn needle() {}\nother\n").unwrap();

        let hit =
            done(grep_search("needle", dir.path(), &json!({}), CancellationToken::new()).await);
        let miss = done(
            grep_search(
                "zzzzznope",
                dir.path(),
                &json!({}),
                CancellationToken::new(),
            )
            .await,
        );

        assert!(
            !hit.is_error && hit.content.contains("needle"),
            "{}",
            hit.content
        );
        // A no-match result is success (a prompt), not an error to retry.
        assert!(!miss.is_error, "no-match must not be an error");
        assert!(miss.content.to_lowercase().contains("no matches"));
    }

    #[tokio::test]
    async fn grep_refuses_a_symlinked_path_argument() {
        // ripgrep does not follow links while *walking*, but it does follow one
        // handed to it as an explicit path argument — so `path: "link"` with
        // `link -> ~` searched the whole home directory under `Permission::None`.
        let (_o, root, home) = fsguard::tests::fixture();
        std::os::unix::fs::symlink(&home, root.join("link")).unwrap();
        let out = done(
            grep_in(
                &root,
                &json!({"pattern": "BEGIN PRIVATE KEY", "path": "link"}),
                CancellationToken::new(),
            )
            .await,
        );
        assert!(out.is_error, "{}", out.content);
        assert!(
            !out.content.contains("id_rsa"),
            "leaked the home directory: {}",
            out.content
        );
        assert!(out.content.contains("symlink"), "{}", out.content);
    }

    #[tokio::test]
    async fn grep_does_not_follow_links_inside_the_tree() {
        let (_o, root, home) = fsguard::tests::fixture();
        std::os::unix::fs::symlink(&home, root.join("src/out")).unwrap();
        let out = done(
            grep_in(
                &root,
                &json!({"pattern": "PRIVATE"}),
                CancellationToken::new(),
            )
            .await,
        );
        assert!(
            !out.content.contains("id_rsa"),
            "rg followed an in-tree link: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn grep_refuses_out_of_workspace_path() {
        let out = GrepTool
            .run(
                json!({"pattern": "x", "path": "/etc"}),
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
