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
/// The most one "line" may ever occupy. A minified bundle is one line.
const READ_MAX_LINE: usize = 8 * 1024;
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

/// The extra write roots granted to `tool` by `[sandbox].file_tools` —
/// empty unless the owner opted the mutating file tools into the
/// `[sandbox].writable` set (`file_tools = "writable"`).
fn granted_extras() -> &'static [std::path::PathBuf] {
    match sandbox::file_tools_mode() {
        sandbox::FileToolsMode::Writable => sandbox::extra_writable(),
        sandbox::FileToolsMode::Workspace => &[],
    }
}

/// Where a mutating file tool's path lands, for both the permission pick and
/// the run-time door — one classification, so the two can never disagree.
enum WriteTarget {
    /// Workspace-relative: the fd descent from the workspace root.
    Workspace(std::path::PathBuf),
    /// Under a granted `[sandbox].writable` root: the same fd descent,
    /// anchored at that root.
    Extra(std::path::PathBuf, std::path::PathBuf),
    /// Outside every boundary: the human approved this literal path in the
    /// protected ask, so it is opened plainly — following links is what they
    /// agreed to.
    OutsideApproved,
}

fn write_target(root: &std::path::Path, extras: &[std::path::PathBuf], path: &str) -> WriteTarget {
    match fsguard::classify(root, path) {
        fsguard::Placement::Inside(rel) => WriteTarget::Workspace(rel),
        fsguard::Placement::Outside(_) => match fsguard::extra_root_for(path, extras) {
            Some((eroot, rel)) => WriteTarget::Extra(eroot, rel),
            None => WriteTarget::OutsideApproved,
        },
    }
}

/// Permission for a mutating file tool: protected paths escalate.
///
/// INVARIANT: the execute-later classification runs on the *resolved* target
/// as well as the literal path, so an innocent-looking name that is a symlink
/// to `~/.zshrc` still gets the escalated ask — a symlink cannot launder a
/// protected write into an ordinary one. Enforced by
/// `file_permission_classifies_the_resolved_target`. That escalation is
/// checked before the extra-root grant, so a granted directory never
/// downgrades a protected filename — enforced by
/// `file_permission_downgrades_extra_paths_only_when_granted`.
fn file_permission(verb: &str, input: &Value) -> Permission {
    file_permission_with(verb, input, granted_extras())
}

fn file_permission_with(verb: &str, input: &Value, extras: &[std::path::PathBuf]) -> Permission {
    let path = input.get("path").and_then(Value::as_str).unwrap_or("?");
    let summary = format!("{verb} {path}");
    let resolved = std::fs::canonicalize(path).ok();
    let target = write_target(fsguard::workspace_root(), extras, path);
    // The write lands on the fsguard-*normalized* path (`Path::components()`
    // has collapsed `//`, `./`, `..`), so classify the protected floor on that
    // same normalized rel — not only the raw model string. Without this,
    // `.github//workflows/x.yml` misses `contains(".github/workflows/")` on the
    // raw string yet still writes into the real protected dir (Vuln 3).
    let normalized_reason = match &target {
        WriteTarget::Workspace(rel) | WriteTarget::Extra(_, rel) => {
            execute_later_reason(&rel.to_string_lossy())
        }
        WriteTarget::OutsideApproved => None,
    };
    let reason = execute_later_reason(path)
        .or_else(|| {
            resolved
                .as_deref()
                .and_then(|r| execute_later_reason(&r.to_string_lossy()))
        })
        .or(normalized_reason);
    match (reason, target) {
        (Some(why), _) => Permission::AskProtected {
            summary,
            why: why.into(),
        },
        (None, WriteTarget::OutsideApproved) => Permission::AskProtected {
            summary,
            why: "outside the working directory: not covered by this tool's workspace boundary"
                .into(),
        },
        (None, WriteTarget::Workspace(_) | WriteTarget::Extra(..)) => Permission::Ask { summary },
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
         most 2000 lines / 200KB per call, and any single line over 8KB is clipped; use \
         `offset`/`limit` to continue a truncated read."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the working directory. An absolute path outside the working directory is permitted but prompts for approval."},
                "offset": {"type": "integer", "description": "1-indexed line to start from (for continuing truncated reads)"},
                "limit": {"type": "integer", "description": "Maximum lines to return (default 2000)"}
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

/// The output window: which lines were kept, and where the next call resumes.
struct ReadWindow {
    out: String,
    offset: usize,
    limit: usize,
    taken: usize,
    last: usize,
    /// Set once the window closed; the line the next call should start at.
    next: Option<usize>,
}

impl ReadWindow {
    fn push(&mut self, lineno: usize, line: &[u8], clipped: bool) {
        if self.next.is_some() || lineno < self.offset {
            return;
        }
        let text = String::from_utf8_lossy(line);
        // Always emit at least one line, however long — it is already capped
        // at READ_MAX_LINE, so "at least one" is bounded too.
        if self.taken > 0 && self.out.len() + text.len() > READ_MAX_BYTES {
            self.next = Some(lineno); // this line did not fit
            return;
        }
        self.out.push_str(&format!("{lineno:>6}\t{text}"));
        if clipped {
            self.out.push_str("…[long line truncated at 8KB]");
        }
        self.out.push('\n');
        self.taken += 1;
        self.last = lineno;
        if self.taken >= self.limit {
            self.next = Some(lineno + 1);
        }
    }
}

/// Append to the line in hand, never past `READ_MAX_LINE`.
///
/// This is the whole of T3-25: the cap applies *while streaming*, so a 4MB
/// "line" costs 8KB of memory instead of 4MB. `BufRead::lines` buffered the
/// entire line first and consulted the byte cap only afterwards — by which
/// point the allocation had already happened, and the result was a window
/// showing nothing at all.
fn stage_line(line: &mut Vec<u8>, clipped: &mut bool, bytes: &[u8]) {
    let room = READ_MAX_LINE.saturating_sub(line.len());
    if bytes.len() > room {
        *clipped = true;
    }
    line.extend_from_slice(&bytes[..room.min(bytes.len())]);
}

/// INVARIANT: no single line ever allocates more than `READ_MAX_LINE`,
/// whatever the file contains, and reading stops as soon as the window is
/// full rather than scanning to EOF for a total nobody asked for. Enforced by
/// `read_caps_a_single_enormous_line` and `read_honors_limit_and_offset`.
///
/// The handle arrives already validated by the guard — this never re-opens by
/// name, which is the whole point of the containment layer.
async fn read_stream(file: tokio::fs::File, path: &str, input: &Value) -> ToolResult {
    use tokio::io::AsyncReadExt;
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(READ_MAX_LINES, |n| (n as usize).clamp(1, READ_MAX_LINES));
    let read_err = |e: std::io::Error| {
        ToolOutcome::err(format!(
            "Could not read `{path}`: {e}. Check the path (use `glob`) and try again."
        ))
    };

    let mut file = file;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut line: Vec<u8> = Vec::new();
    let mut clipped = false;
    let mut lineno = 0usize;
    let mut eof = false;
    let mut w = ReadWindow {
        out: String::new(),
        offset,
        limit,
        taken: 0,
        last: 0,
        next: None,
    };

    'read: while w.next.is_none() {
        let n = file.read(&mut chunk).await.map_err(read_err)?;
        if n == 0 {
            eof = true;
            break;
        }
        let mut rest = &chunk[..n];
        while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
            stage_line(&mut line, &mut clipped, &rest[..nl]);
            rest = &rest[nl + 1..];
            lineno += 1;
            w.push(lineno, &line, clipped);
            line.clear();
            clipped = false;
            if w.next.is_some() {
                break 'read;
            }
        }
        stage_line(&mut line, &mut clipped, rest);
    }
    // A trailing run with no final newline is still a line.
    if w.next.is_none() && (!line.is_empty() || clipped) {
        lineno += 1;
        w.push(lineno, &line, clipped);
    }

    if eof && offset > lineno && lineno > 0 {
        return Err(ToolOutcome::err(format!(
            "`{path}` has only {lineno} lines; offset {offset} is past the end."
        )));
    }
    // Totals are only reported when EOF was actually reached — stopping early
    // is the point, and claiming a total we never counted would be a lie.
    if let Some(next) = w.next {
        if !(eof && next > lineno) {
            let total = if eof {
                format!(" of {lineno}")
            } else {
                String::new()
            };
            w.out.push_str(&format!(
                "\n[truncated: showing lines {offset}-{}{total}; continue with offset={next}]",
                w.last
            ));
        }
    }
    if w.out.is_empty() {
        w.out = "[empty file]".into();
    }
    let out = w.out;
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
            let result = write_in(root, granted_extras(), &input).await;
            with_diagnostics(&self.diag, &input, result).await
        })
    }
}

/// INVARIANT: a write inside the workspace — or under a granted
/// `[sandbox].writable` root — goes through the guarded create, so no
/// component of the path, including the final one, is ever a symlink that
/// would redirect the bytes somewhere else. A path outside every boundary
/// was approved explicitly by the human, who saw the resolved target in the
/// ask, so it is written plainly. Enforced by
/// `write_through_a_symlink_does_not_touch_the_target` and
/// `write_into_an_extra_root_goes_through_the_guard`.
async fn write_in(
    root: &std::path::Path,
    extras: &[std::path::PathBuf],
    input: &Value,
) -> ToolResult {
    let path = str_arg(input, "path")?;
    let content = str_arg(input, "content")?;
    match write_target(root, extras, path) {
        WriteTarget::Workspace(rel) => guarded_write_bytes(root, &rel, path, content)?,
        WriteTarget::Extra(eroot, rel) => guarded_write_bytes(&eroot, &rel, path, content)?,
        WriteTarget::OutsideApproved => {
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

/// The guarded create-and-write, shared by the workspace and extra-root arms.
fn guarded_write_bytes(
    root: &std::path::Path,
    rel: &std::path::Path,
    path: &str,
    content: &str,
) -> Result<(), ToolOutcome> {
    use std::io::Write;
    let mut f = fsguard::create_beneath(root, rel, true)
        .map_err(|e| ToolOutcome::err(e.prompt("write", path)))?;
    f.write_all(content.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| ToolOutcome::err(format!("Could not write `{path}`: {e}.")))
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
            let result = edit_in(root, granted_extras(), &input).await;
            with_diagnostics(&self.diag, &input, result).await
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
async fn edit_in(
    root: &std::path::Path,
    extras: &[std::path::PathBuf],
    input: &Value,
) -> ToolResult {
    edit_with_hook(root, extras, input, || {}).await
}

/// `between` runs after the match and before the write-back — the window an
/// external writer would land in. Production passes a no-op; the test seam
/// passes a real mutation.
async fn edit_with_hook(
    root: &std::path::Path,
    extras: &[std::path::PathBuf],
    input: &Value,
    between: impl FnOnce(),
) -> ToolResult {
    let path = str_arg(input, "path")?;
    let old = str_arg(input, "old_string")?;
    let new = str_arg(input, "new_string")?;
    if old.is_empty() {
        return Err(ToolOutcome::err(
            "`old_string` is empty. Use `write` to create a file, or provide the exact text to replace.",
        ));
    }
    let target = write_target(root, extras, path);
    let content = read_guarded_to_string(root, &target, path)?;
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
            between();
            // INVARIANT: an edit either applies to exactly the bytes it matched
            // or it fails loudly — a concurrent external write is never
            // silently overwritten. Enforced by
            // `edit_detects_a_concurrent_modification_instead_of_losing_it`.
            //
            // INVARIANT (partial): the compare and the rename are not one
            // operation. Concurrent modification is *detected*
            // (compare-then-replace) and the write is *atomic* (tmp + fsync +
            // renameat + parent fsync), but a writer landing between the
            // re-read and the `renameat` is still lost. That window is
            // microseconds, not zero. Closing it fully needs either an
            // advisory lock (not portable enough to rely on, and no other
            // process in the workspace takes one) or edit-after-read staleness
            // tracking in the engine — tracked in the tech-debt tracker.
            let now = read_guarded_to_string(root, &target, path)?;
            if now != content {
                return Err(ToolOutcome::err(format!(
                    "`{path}` changed on disk between the match and the write, so the edit was \
                     not applied (nothing was lost). Re-read the file and re-issue the edit \
                     against the current text."
                )));
            }
            write_guarded(root, &target, path, updated.as_bytes())?;
            let note = if exact {
                ""
            } else {
                " (whitespace-tolerant match; the file's indentation was preserved)"
            };
            Ok(ToolOutcome::ok(format!("Edited {path}.{note}")))
        }
    }
}

/// Read a file for editing through whichever door its `WriteTarget` opened:
/// the fd descent inside the workspace or a granted extra root, a plain open
/// outside every boundary (which the human approved by path).
fn read_guarded_to_string(
    root: &std::path::Path,
    target: &WriteTarget,
    path: &str,
) -> Result<String, ToolOutcome> {
    let not_read = |e: std::io::Error| {
        ToolOutcome::err(format!(
            "Could not read `{path}`: {e}. Read the file first to confirm the path."
        ))
    };
    let guarded_read = |root: &std::path::Path, rel: &std::path::Path| {
        use std::io::Read;
        let mut f = fsguard::open_beneath(root, rel)
            .map_err(|e| ToolOutcome::err(e.prompt("edit", path)))?;
        let mut s = String::new();
        f.read_to_string(&mut s).map_err(not_read)?;
        Ok(s)
    };
    match target {
        WriteTarget::Workspace(rel) => guarded_read(root, rel),
        WriteTarget::Extra(eroot, rel) => guarded_read(eroot, rel),
        WriteTarget::OutsideApproved => std::fs::read_to_string(path).map_err(not_read),
    }
}

/// The write-back half of `read_guarded_to_string`. Inside the workspace or a
/// granted extra root this is the atomic replace (temp sibling, fsync,
/// `renameat`, parent fsync); outside every boundary, a plain whole-file
/// write.
fn write_guarded(
    root: &std::path::Path,
    target: &WriteTarget,
    path: &str,
    bytes: &[u8],
) -> Result<(), ToolOutcome> {
    match target {
        WriteTarget::Workspace(rel) => fsguard::replace_beneath(root, rel, bytes)
            .map_err(|e| ToolOutcome::err(e.prompt("edit", path))),
        WriteTarget::Extra(eroot, rel) => fsguard::replace_beneath(eroot, rel, bytes)
            .map_err(|e| ToolOutcome::err(e.prompt("edit", path))),
        WriteTarget::OutsideApproved => std::fs::write(path, bytes)
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
        .kill_on_drop(true);
    // Own session + process group (pgid == pid): detaches the controlling
    // terminal (Vuln 2) and keeps kill_group's kill(-pid) reaping the group.
    sandbox::detach_session(&mut cmd);
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

/// The `>`/`>>` redirect targets in a command line, quote-aware. Skips fd
/// duplications (`2>&1`, `>&2`), which name a descriptor, not a file.
fn redirect_targets(cmd: &str) -> Vec<String> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut targets = Vec::new();
    let mut i = 0;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                i += 1;
            }
            '>' => {
                i += 1;
                if i < chars.len() && chars[i] == '>' {
                    i += 1; // `>>`
                }
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                if i < chars.len() && chars[i] == '&' {
                    continue; // `>&fd` / `2>&1` — no file named
                }
                let mut t = String::new();
                let mut tq: Option<char> = None;
                while i < chars.len() {
                    let ch = chars[i];
                    match tq {
                        Some(q) if ch == q => tq = None,
                        Some(_) => t.push(ch),
                        None if ch == '\'' || ch == '"' => tq = Some(ch),
                        None if ch.is_whitespace()
                            || matches!(ch, '|' | ';' | '&' | '<' | '>' | '(' | ')') =>
                        {
                            break
                        }
                        None => t.push(ch),
                    }
                    i += 1;
                }
                if !t.is_empty() {
                    targets.push(t);
                }
            }
            _ => i += 1,
        }
    }
    targets
}

/// The paths a bash command *writes*: `>`/`>>` redirect targets plus the file
/// arguments of the common file-writing commands (`tee`, `dd of=`). bash is not
/// fully statically analyzable; on macOS the kernel floor denies a protected
/// write regardless, so a miss here degrades to that backstop, not to a silent
/// write. Linux carries only this check until the deferred Landlock work lands.
fn bash_write_targets(cmd: &str) -> Vec<String> {
    let mut targets = redirect_targets(cmd);
    for seg in crate::rules::shell_segments(cmd) {
        let toks: Vec<String> = crate::rules::tokenize(seg)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let Some(first) = toks.first() else { continue };
        match first.rsplit('/').next().unwrap_or(first) {
            "tee" => targets.extend(toks[1..].iter().filter(|t| !t.starts_with('-')).cloned()),
            "dd" => targets.extend(
                toks[1..]
                    .iter()
                    .filter_map(|t| t.strip_prefix("of=").map(str::to_string)),
            ),
            _ => {}
        }
    }
    targets
}

/// The execute-later reason if any path this bash command *writes* is protected
/// (Vuln 4). Reads are deliberately not escalated — the floor is about writes,
/// so ordinary work stays out of the way.
fn bash_protected_write_reason(cmd: &str) -> Option<&'static str> {
    bash_write_targets(cmd).into_iter().find_map(|t| {
        let normalized: std::path::PathBuf = std::path::Path::new(&t).components().collect();
        execute_later_reason(&normalized.to_string_lossy())
    })
}

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command (`sh -c`). Default timeout 120s (`timeout_ms` overrides, max 600s); \
         the whole process group is killed on timeout or cancel. stdout and stderr share one pipe, \
         so output arrives in the order the command actually wrote it (as a terminal shows it) and \
         the two are not distinguishable. Truncated at 50KB. A failure ends with a structured \
         trailer: `[exit N]` or `[killed by SIGNAME]`."
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
        let summary = format!("bash [{label}]: {short}");
        // Vuln 4: a bash command that writes an execute-later path escalates to
        // the protected ask (which auto-mode cannot skip), just like the `write`
        // tool. Reads stay an ordinary Ask, so normal work is unaffected.
        if let Some(why) = bash_protected_write_reason(cmd) {
            return Permission::AskProtected {
                summary,
                why: format!("bash writes a protected path — {why}"),
            };
        }
        Permission::Ask { summary }
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
    let (child_out, child_err, pipe) = merged_pipe()
        .map_err(|e| ToolOutcome::err(format!("Could not create the output pipe: {e}.")))?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(child_out)
        .stderr(child_err)
        .kill_on_drop(true);
    // Own session + process group (pgid == pid): detaches the controlling
    // terminal (Vuln 2) and keeps kill_group's kill(-pid) reaping the group.
    sandbox::detach_session(&mut cmd);
    let child = cmd
        .spawn()
        .map_err(|e| ToolOutcome::err(format!("Could not start shell: {e}.")))?;
    // `Command::spawn` takes `&mut self`, so `cmd` still owns the `Stdio`
    // values — i.e. the parent's own copies of the write ends. Until they are
    // dropped there is a writer left on the pipe and the drain below never
    // sees EOF. `Stdio::piped()` hides this because std keeps the parent end
    // on the `Child`; handing in our own fds makes it ours to release.
    drop(cmd);
    let pid = child.id();
    let wait = collect_merged(child, pipe, BASH_MAX_OUTPUT + BASH_OUTPUT_SLACK);
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

/// One pipe, handed to the child as both stdout and stderr.
///
/// INVARIANT: the child's stdout and stderr land in a single pipe in the order
/// the child wrote them, exactly as a terminal shows them. Two pipes read
/// separately and concatenated structurally cannot do this — the causal order
/// is destroyed before anyone sees it. The cost is that the two streams are no
/// longer distinguishable; that is the right trade, because causal order is
/// what the model reasons about. Enforced by
/// `bash_preserves_stdout_stderr_interleaving`.
fn merged_pipe() -> std::io::Result<(
    std::process::Stdio,
    std::process::Stdio,
    tokio::net::unix::pipe::Receiver,
)> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a live two-element array, which is what pipe(2) writes.
    // `pipe2(O_CLOEXEC)` is Linux-only but closes the inherit race outright;
    // elsewhere the fcntls below run immediately after, leaving only the
    // window std itself has on those platforms.
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both fds are freshly created and owned by nothing else.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // BOTH ends need CLOEXEC, and the write end is the subtle one: any *other*
    // process this program spawns concurrently would otherwise inherit a copy
    // and hold the pipe open, so this drain would not see EOF until that
    // unrelated process exited. The read end matters for the same reason, one
    // level closer. `dup2` onto fd 1/2 clears CLOEXEC on the copies the child
    // actually gets, so setting it here costs the child nothing.
    set_cloexec(&read_end)?;
    set_cloexec(&write_end)?;
    // `try_clone` dups with CLOEXEC already set.
    let write_end2 = write_end.try_clone()?;
    // Sets O_NONBLOCK itself; must be called inside the runtime.
    let rx = tokio::net::unix::pipe::Receiver::from_owned_fd(read_end)?;
    Ok((
        std::process::Stdio::from(write_end),
        std::process::Stdio::from(write_end2),
        rx,
    ))
}

fn set_cloexec(fd: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: plain fcntl(2) on a fd we own.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same fd, adding one flag.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Drain the merged pipe to EOF (capped at `cap` bytes), then reap the child.
/// Draining first is what keeps a chatty command from blocking on a full pipe
/// while we wait for it to exit.
///
/// Additive beside `collect_output`, whose signature is shared with the
/// diagnostics runner and is deliberately left alone.
async fn collect_merged(
    mut child: tokio::process::Child,
    pipe: tokio::net::unix::pipe::Receiver,
    cap: usize,
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>)> {
    let bytes = drain_capped(Some(pipe), cap).await;
    let status = child.wait().await?;
    Ok((status, bytes))
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

fn shell_outcome(result: std::io::Result<(std::process::ExitStatus, Vec<u8>)>) -> ToolOutcome {
    let (status, bytes) = match result {
        Ok(o) => o,
        Err(e) => return ToolOutcome::err(format!("Failed waiting on command: {e}.")),
    };
    let text = clip_output(&bytes);
    if status.success() {
        return ToolOutcome::ok(if text.is_empty() {
            "(no output)".to_string()
        } else {
            text
        });
    }
    // A structured trailer so the model can branch on *why* it failed without
    // parsing the prose. (A typed field on `ToolOutcome` would be better still,
    // but that type crosses into the engine and the surface — tracker #17's
    // neighbour, deferred with the same reasoning.)
    use std::os::unix::process::ExitStatusExt;
    let (label, trailer) = match (status.code(), status.signal()) {
        (Some(code), _) => (format!("status {code}"), format!("[exit {code}]")),
        (None, Some(sig)) => (
            format!("signal {}", signal_name(sig)),
            format!("[killed by {}]", signal_name(sig)),
        ),
        (None, None) => (
            "an unknown status".to_string(),
            "[exit unknown]".to_string(),
        ),
    };
    ToolOutcome::err(format!("Command exited with {label}.\n{text}\n{trailer}"))
}

fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGHUP => "SIGHUP".into(),
        libc::SIGINT => "SIGINT".into(),
        libc::SIGQUIT => "SIGQUIT".into(),
        libc::SIGILL => "SIGILL".into(),
        libc::SIGABRT => "SIGABRT".into(),
        libc::SIGFPE => "SIGFPE".into(),
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGSEGV => "SIGSEGV".into(),
        libc::SIGPIPE => "SIGPIPE".into(),
        libc::SIGALRM => "SIGALRM".into(),
        libc::SIGTERM => "SIGTERM".into(),
        other => format!("signal {other}"),
    }
}

fn clip_output(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
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
///
/// INVARIANT: this is only ever called while the `Child` is still owned and
/// un-reaped by the pending `collect_merged`/`collect_output` future (the
/// `tokio::select!` timeout and cancel arms in `bash_impl` and `grep_search`),
/// so the pid is either live or a zombie — reserved either way, and never
/// reusable by another process. Killing after the child had been waited on
/// would be a pid-reuse bug: the number could by then name someone else's
/// process, and the negation would name someone else's *group*. That is why
/// the kill happens before the wait future is dropped, not after. Its
/// observable half — the whole group really dies — is enforced by
/// `timeout_kills_the_whole_process_group`.
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
    async fn read_caps_a_single_enormous_line() {
        let (_o, root, _home) = fsguard::tests::fixture();
        // No newline at all — a minified bundle or a binary blob.
        std::fs::write(root.join("bundle.js"), "x".repeat(4 * 1024 * 1024)).unwrap();
        let out = done(read_in(&root, &json!({"path": "bundle.js"})).await);
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.len() < 300 * 1024,
            "buffered the whole blob: {}",
            out.content.len()
        );
        assert!(
            out.content.contains("long line truncated"),
            "{}",
            out.content
        );
        // ...and the cap *shows* the head of the line rather than dropping the
        // file whole, which is what the old whole-line buffer ended up doing.
        assert!(out.content.contains(&"x".repeat(1000)));
    }

    #[tokio::test]
    async fn read_honors_limit_and_offset() {
        let (_o, root, _home) = fsguard::tests::fixture();
        let body: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(root.join("many.txt"), &body).unwrap();
        let out = done(
            read_in(
                &root,
                &json!({"path": "many.txt", "offset": 50, "limit": 10}),
            )
            .await,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("line 50") && out.content.contains("line 59"));
        assert!(
            !out.content.contains("line 49") && !out.content.contains("line 60"),
            "window leaked: {}",
            out.content
        );
        assert!(
            out.content.contains("offset=60"),
            "must hint the next window: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn write_through_a_symlink_does_not_touch_the_target() {
        let (_o, root, home) = fsguard::tests::fixture();
        let target = home.join(".zshrc");
        std::fs::write(&target, "# original\n").unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::os::unix::fs::symlink(&target, root.join("docs/notes.md")).unwrap();

        let out = done(
            write_in(
                &root,
                &[],
                &json!({"path": "docs/notes.md", "content": "evil"}),
            )
            .await,
        );
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

    #[test]
    fn file_permission_normalizes_before_the_protected_check() {
        // Vuln 3: a doubled separator collapses to the real protected dir on
        // write, so the floor must classify the normalized path, not the raw
        // string that `contains(".github/workflows/")` never matches.
        match file_permission("write", &json!({"path": ".github//workflows/ci.yml"})) {
            Permission::AskProtected { why, .. } => assert!(why.contains("CI workflow"), "{why}"),
            other => panic!("a separator trick must not dodge the floor, got {other:?}"),
        }
        assert!(
            matches!(
                file_permission("write", &json!({"path": ".git/./config"})),
                Permission::AskProtected { .. }
            ),
            "a `.` segment must not dodge the floor either"
        );
    }

    #[test]
    fn file_permission_is_case_insensitive_for_protected_basenames() {
        // Vuln 3 (case half): a case-insensitive volume resolves `MAKEFILE` to
        // `Makefile`, so a byte-exact basename match cannot be the only guard.
        match file_permission("write", &json!({"path": "MAKEFILE"})) {
            Permission::AskProtected { why, .. } => {
                assert!(why.contains("build entrypoint"), "{why}")
            }
            other => panic!("MAKEFILE must escalate like Makefile, got {other:?}"),
        }
        assert!(matches!(
            file_permission("write", &json!({"path": "BUILD.RS"})),
            Permission::AskProtected { .. }
        ));
    }

    #[test]
    fn bash_escalates_a_write_to_a_protected_path() {
        // Vuln 4: bash writing an execute-later path must escalate to the
        // protected ask (which auto-mode cannot skip), while ordinary commands
        // — including *reading* a protected file — stay an ordinary Ask so the
        // floor stays out of the way for normal work.
        let protected = |cmd: &str| {
            matches!(
                BashTool.permission(&json!({ "command": cmd })),
                Permission::AskProtected { .. }
            )
        };
        assert!(protected("echo hi > .git/hooks/pre-commit"), "redirect");
        assert!(protected("cat > Makefile"), "redirect to a basename");
        assert!(
            protected("echo x >> .github//workflows/ci.yml"),
            "normalized"
        );
        assert!(protected("echo x | tee .git/hooks/pre-push"), "tee");
        let ordinary = |cmd: &str| {
            matches!(
                BashTool.permission(&json!({ "command": cmd })),
                Permission::Ask { .. }
            )
        };
        assert!(ordinary("cargo test"), "unrelated command");
        assert!(
            ordinary("cat Makefile"),
            "reading a protected file is not a write"
        );
        assert!(
            ordinary("ls -la .github/workflows"),
            "listing is not a write"
        );
    }

    #[test]
    fn file_permission_downgrades_extra_paths_only_when_granted() {
        let (_o, _root, home) = fsguard::tests::fixture();
        let extra = home.join("cache");
        std::fs::create_dir_all(&extra).unwrap();
        let extras = vec![extra.clone()];
        let input = json!({"path": extra.join("blob.bin").to_str().unwrap()});
        // Granted: an ordinary ask, the same tier as an in-workspace write
        // (auto-approved under mode=auto — that is the point of the grant).
        assert!(matches!(
            file_permission_with("write", &input, &extras),
            Permission::Ask { .. }
        ));
        // Not granted: the protected ask, exactly as before.
        assert!(matches!(
            file_permission_with("write", &input, &[]),
            Permission::AskProtected { .. }
        ));
        // A protected filename under a granted extra still escalates — the
        // execute-later class outranks the grant.
        let protected = json!({"path": extra.join("Makefile").to_str().unwrap()});
        match file_permission_with("write", &protected, &extras) {
            Permission::AskProtected { why, .. } => {
                assert!(why.contains("build entrypoint"), "{why}")
            }
            other => panic!("protected name must stay escalated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_into_an_extra_root_goes_through_the_guard() {
        let (_o, root, home) = fsguard::tests::fixture();
        let extra = home.join("cache");
        std::fs::create_dir_all(&extra).unwrap();
        let extras = vec![extra.clone()];

        // The write lands, parents created, through the fd descent.
        let p = extra.join("a/b.txt");
        let ok = done(
            write_in(
                &root,
                &extras,
                &json!({"path": p.to_str().unwrap(), "content": "x"}),
            )
            .await,
        );
        assert!(!ok.is_error, "{}", ok.content);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x");

        // A symlink under the extra pointing out of it is refused — the
        // grant is not a plain-write door, the guard still descends.
        let victim = home.join(".zshrc");
        std::fs::write(&victim, "# original\n").unwrap();
        std::os::unix::fs::symlink(&victim, extra.join("innocent.txt")).unwrap();
        let out = done(
            write_in(
                &root,
                &extras,
                &json!({"path": extra.join("innocent.txt").to_str().unwrap(), "content": "evil"}),
            )
            .await,
        );
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("symlink"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "# original\n");
    }

    #[tokio::test]
    async fn edit_in_an_extra_root_round_trips() {
        let (_o, root, home) = fsguard::tests::fixture();
        let extra = home.join("cache");
        std::fs::create_dir_all(&extra).unwrap();
        let p = extra.join("conf.txt");
        std::fs::write(&p, "value = 1\n").unwrap();
        let ok = done(
            edit_in(
                &root,
                std::slice::from_ref(&extra),
                &json!({
                    "path": p.to_str().unwrap(),
                    "old_string": "value = 1",
                    "new_string": "value = 2",
                }),
            )
            .await,
        );
        assert!(!ok.is_error, "{}", ok.content);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "value = 2\n");
    }

    #[tokio::test]
    async fn write_creates_parents_but_never_through_a_link() {
        let (_o, root, home) = fsguard::tests::fixture();
        std::os::unix::fs::symlink(&home, root.join("out")).unwrap();
        let out = done(write_in(&root, &[], &json!({"path": "out/a/b.txt", "content": "x"})).await);
        assert!(out.is_error, "{}", out.content);
        assert!(!home.join("a/b.txt").exists(), "created through the link");
        // The ordinary case still works.
        let ok = done(write_in(&root, &[], &json!({"path": "a/b/c.txt", "content": "x"})).await);
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
                &[],
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
    async fn edit_detects_a_concurrent_modification_instead_of_losing_it() {
        let (_o, root, _home) = fsguard::tests::fixture();
        let p = root.join("f.txt");
        std::fs::write(&p, "aaa\nbbb\nccc\n").unwrap();
        let external = "aaa\nbbb\nEXTERNAL\n";
        // The seam fires between the match and the write-back — exactly where
        // an external writer would land.
        let out = done(
            edit_with_hook(
                &root,
                &[],
                &json!({"path": "f.txt", "old_string": "bbb", "new_string": "BBB"}),
                || std::fs::write(&p, external).unwrap(),
            )
            .await,
        );
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("changed on disk") && out.content.contains("Re-read"),
            "must be a prompt: {}",
            out.content
        );
        // The external change survives intact — the edit did not clobber it.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), external);
    }

    #[tokio::test]
    async fn edit_keeps_the_files_indentation_on_a_tolerant_match() {
        let (_o, root, _home) = fsguard::tests::fixture();
        // File uses tabs; the model reproduces the block with spaces.
        std::fs::write(root.join("f.rs"), "fn f() {\n\tif x {\n\t\ta();\n\t}\n}\n").unwrap();
        let ok = done(
            edit_in(
                &root,
                &[],
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
                &[],
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
    fn bash_preserves_stdout_stderr_interleaving() {
        // Two pipes concatenated structurally cannot do this: the model would
        // see all of stdout, then all of stderr, and reason about a sequence
        // that never happened.
        let out = run(
            &BashTool,
            json!({"command": "printf 'one\\n'; printf 'two\\n' >&2; printf 'three\\n'"}),
        );
        assert!(!out.is_error, "{}", out.content);
        let (a, b, c) = (
            out.content.find("one").unwrap(),
            out.content.find("two").unwrap(),
            out.content.find("three").unwrap(),
        );
        assert!(a < b && b < c, "interleaving lost: {}", out.content);
    }

    #[test]
    fn bash_reports_the_exit_status_structurally() {
        let f = run(&BashTool, json!({"command": "exit 3"}));
        assert!(
            f.is_error && f.content.contains("status 3"),
            "{}",
            f.content
        );
        assert!(f.content.contains("[exit 3]"), "{}", f.content);
        let k = run(&BashTool, json!({"command": "kill -TERM $$"}));
        assert!(k.is_error && k.content.contains("signal"), "{}", k.content);
        assert!(k.content.contains("[killed by SIGTERM]"), "{}", k.content);
    }

    #[test]
    fn timeout_kills_the_whole_process_group() {
        // `kill_group` targets -pid, so it reaches the child's own children.
        // The invariant it depends on is that the kill lands while the `Child`
        // is still owned and un-reaped, so the pid is still reserved and can
        // never name somebody else's process.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("still-alive");
        let cmd = format!("(sleep 1; touch {}) & sleep 5", marker.to_str().unwrap());
        let t = run(&BashTool, json!({"command": cmd, "timeout_ms": 300}));
        assert!(
            t.is_error && t.content.contains("timed out"),
            "{}",
            t.content
        );
        // Outlive the background sleep: if the group kill had missed it, the
        // marker would appear after the tool call already returned.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        assert!(
            !marker.exists(),
            "a grandchild survived the process-group kill"
        );
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
