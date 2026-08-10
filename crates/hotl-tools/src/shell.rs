//! Which POSIX shell the `bash` tool runs commands in.
//!
//! # Why the language stays POSIX, and the tokenizer is never forked
//!
//! `rules.rs` consumes POSIX shell grammar in eight functions to enforce deny
//! rules — `unanalyzable`'s `$`/backtick veto, `shell_segments`, `tokenize`,
//! `segment_argvs`, and the rest — plus `bash_write_targets` and
//! `redirect_targets` in `builtins.rs`. Swapping the interpreter would mean
//! re-deriving every one of them against a different grammar. Two candidates
//! get proposed and both are rejected on **security** grounds, not taste:
//!
//! - **`cmd /c`.** `%VAR%` and delayed `!VAR!` expansion are invisible to
//!   `unanalyzable`'s veto, so `set X=curl& %X% evil.com` walks straight past a
//!   `curl` deny rule — reopening the exact class that veto exists to close.
//!   Single quotes are not quotes in `cmd`, so `tokenize`'s model is wrong in
//!   the **unsafe** direction. And `cmd /c "…"` quote-stripping is its own CVE
//!   family (the BatBadBut class, CVE-2024-24576).
//! - **`powershell -Command`.** Some accidents favor it — `;` really is a
//!   separator, `$` really is expansion — but backtick is PowerShell's *escape*
//!   character, so `unanalyzable` would fire on every escaped character, and
//!   **aliases break `basename` at the root**: `curl` is an alias for
//!   `Invoke-WebRequest` in Windows PowerShell 5.1, so a `curl` deny rule
//!   denies a program that is not `curl` while missing `iwr`. `segment_argvs`'
//!   whole premise — that argv[0] reduced to a basename identifies the command
//!   — does not hold in a language with a user-extensible alias table. Plus
//!   200–400 ms of startup per call.
//!
//! So the interpreter becomes a **checked precondition** instead. Every shell
//! this module will resolve is POSIX, and `rules.rs` changes not one line.
//!
//! The decisive argument is Windows-specific. On Unix a `bash_write_targets`
//! miss "degrades to that backstop, not to a silent write" — the kernel floor
//! catches it. Until the Windows floor lands, `probe()` returns `Unavailable`
//! there, so `sandbox_enforced` is false and **there is no backstop at all**. A
//! miss *is* a silent write. Handing the model a grammar hotl cannot analyze
//! would be doing that on purpose.
//!
//! Rejected and recorded: bundling busybox-w32 is technically the best fallback
//! (real POSIX `ash`, ~700 KB, and unlike MSYS no path translation at all) but
//! it is GPLv2-only against hotl's AGPL-3.0-or-later. Invoking a separate
//! executable is aggregation rather than linking, so it is *possible* — it
//! needs a written licensing position and a `THIRD-PARTY-NOTICES` entry first.
//! Left documented as the escape hatch if the Git-Bash prerequisite proves too
//! painful in the field.

use std::path::PathBuf;
use std::sync::OnceLock;

/// A resolved POSIX shell, and how it was found — the "how" is what a `doctor`
/// row needs to tell a user what to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSpec {
    pub path: PathBuf,
    /// A short tag for the ask label and `doctor`: `path`, `git-bash`,
    /// `msys2`, or `env`.
    pub how: &'static str,
}

impl ShellSpec {
    /// What the ask label shows. Empty on Unix, where `sh` is not news.
    pub fn label(&self) -> String {
        if cfg!(unix) {
            String::new()
        } else {
            format!(" shell:{}", self.how)
        }
    }
}

/// Memoized like `probe()` — one resolution per process.
pub fn resolve_shell() -> Result<&'static ShellSpec, &'static str> {
    static RESOLVED: OnceLock<Result<ShellSpec, &'static str>> = OnceLock::new();
    RESOLVED
        .get_or_init(resolve_uncached)
        .as_ref()
        .map_err(|e| *e)
}

fn resolve_uncached() -> Result<ShellSpec, &'static str> {
    // An explicit override wins everywhere, so a user with a shell in an odd
    // place is never stuck.
    if let Some(p) = std::env::var_os("HOTL_SHELL").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(ShellSpec { path, how: "env" });
        }
        return Err("$HOTL_SHELL is set but does not name a file");
    }

    #[cfg(unix)]
    {
        // `sh` is guaranteed by POSIX. Naming it rather than searching keeps
        // the pre-port behavior byte-identical.
        Ok(ShellSpec {
            path: PathBuf::from("sh"),
            how: "path",
        })
    }
    #[cfg(windows)]
    {
        if let Some(path) = on_path("sh.exe") {
            return Ok(ShellSpec { path, how: "path" });
        }
        for (base, how) in windows_candidates() {
            let candidate = base.join(r"usr\bin\sh.exe");
            if candidate.is_file() {
                return Ok(ShellSpec {
                    path: candidate,
                    how,
                });
            }
            let candidate = base.join(r"bin\sh.exe");
            if candidate.is_file() {
                return Ok(ShellSpec {
                    path: candidate,
                    how,
                });
            }
        }
        Err(
            "no POSIX shell was found. Install Git for Windows (which ships one) or MSYS2, or \
             point $HOTL_SHELL at an `sh.exe`. hotl will not fall back to `cmd` or PowerShell: \
             neither can be analyzed by the deny rules, so a rule would silently stop applying.",
        )
    }
}

/// Search `PATH` by hand rather than letting `CreateProcess` do it.
///
/// The difference is load-bearing on Windows: `CreateProcess` also searches the
/// **current directory** before `PATH`, so a workspace containing `sh.exe` — a
/// file the model may have just written — would win. Resolving to an absolute
/// path here closes that.
#[cfg(windows)]
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
}

/// Git for Windows and MSYS2, by env var and then by the registry key the
/// installer writes.
#[cfg(windows)]
fn windows_candidates() -> Vec<(PathBuf, &'static str)> {
    let mut out = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(base) = std::env::var_os(var) {
            out.push((PathBuf::from(&base).join("Git"), "git-bash"));
        }
    }
    for root in [r"C:\msys64", r"C:\msys32"] {
        out.push((PathBuf::from(root), "msys2"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On Unix this is `sh` and always has been; the point of the test is that
    /// the resolution exists at all and is memoized, so `build_command` never
    /// pays for it twice.
    #[test]
    fn shell_resolution_is_memoized() {
        let a = resolve_shell();
        let b = resolve_shell();
        assert_eq!(a.is_ok(), b.is_ok());
        if let (Ok(a), Ok(b)) = (a, b) {
            assert!(
                std::ptr::eq(a, b),
                "resolution must happen once per process"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn shell_is_plain_sh_on_unix_and_the_label_stays_silent() {
        let spec = resolve_shell().unwrap();
        assert_eq!(spec.path, PathBuf::from("sh"));
        // Silence is the hardened default: a marker appears only where
        // something was lifted, and on Unix nothing was.
        assert_eq!(spec.label(), "");
    }

    /// The registry filter is the mechanism, not a bespoke branch: this is the
    /// same `Registry::filtered` the browser profile is promised to use.
    #[test]
    fn shell_absence_drops_bash_from_the_registry() {
        let registry = crate::Registry::builtin();
        let with_shell = crate::filter_for_shell(registry, true);
        assert!(with_shell.get("bash").is_some());

        let registry = crate::Registry::builtin();
        let without = crate::filter_for_shell(registry, false);
        assert!(
            without.get("bash").is_none(),
            "with no POSIX shell the bash tool must be absent, not silently \
             pointed at a different language"
        );
        // The file tools are untouched — losing the shell is not losing hotl.
        for still in ["read", "write", "edit", "glob", "grep"] {
            assert!(without.get(still).is_some(), "`{still}` must survive");
        }
    }
}
