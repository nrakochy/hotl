//! One path vocabulary for the string-shaped path handling in the rules engine
//! and the write classifier.
//!
//! These take `&str`, not `Path`, deliberately. Their inputs are model-authored
//! tool arguments and user-authored rule prefixes, and both are POSIX-shaped
//! even on Windows — a command that runs under Git Bash says `/c/Users/you`,
//! not `C:\Users\you`. `std::path` on Windows would answer questions about
//! those strings differently from the shell that actually runs them.
//!
//! The rule that keeps this honest: **a separator folds only where the OS has a
//! second separator.** On Unix a backslash is an ordinary filename character, so
//! folding it would invent matches against files that do not exist. Every
//! function here is therefore byte-identical to the `'/'`-only code it replaced
//! when compiled for Unix.

use std::borrow::Cow;

/// The separators this platform recognizes. Windows accepts both; Unix accepts
/// one, and a backslash there is a filename character.
pub const SEPARATORS: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };

/// Rewrite `\` to `/` where the OS treats them alike, so everything downstream
/// can match on one separator. A no-op (and an allocation-free `Borrowed`) on
/// Unix.
pub fn normalize_separators(path: &str) -> Cow<'_, str> {
    if cfg!(windows) && path.contains('\\') {
        Cow::Owned(path.replace('\\', "/"))
    } else {
        Cow::Borrowed(path)
    }
}

/// Whether a path string is anchored rather than resolved against *our* working
/// directory.
///
/// A leading `/` counts on every platform. The Windows-only forms are
/// drive-absolute (`C:\x`), UNC (`\\srv\share`), verbatim (`\\?\C:\x`),
/// root-relative (`\x`), and **drive-relative (`C:x`)**. The last is not
/// absolute in any strict sense — it resolves against that drive's own current
/// directory — but it does not resolve against ours either, so containment code
/// must treat it as anchored. Calling it relative is the fail-open answer.
pub fn is_absolute_str(path: &str) -> bool {
    !root_prefix(path).is_empty()
}

/// The anchoring prefix of `path`, or `""` when it is relative.
///
/// Returned as a slice rather than a bool so a caller that strips the root can
/// put the *same* root back — `/x` re-roots as `/`, but `C:/x` must not re-root
/// as `/c:/x`. Expects [`normalize_separators`] to have run.
pub fn root_prefix(path: &str) -> &str {
    if path.starts_with('/') {
        if cfg!(windows) && path.starts_with("//") {
            // UNC `//srv/share/…` and verbatim `//?/C:/…`: the root runs
            // through the second component. Anything shorter is a degenerate
            // form — return all of it rather than let a partial UNC path read
            // as relative and match a floating deny prefix at the wrong depth.
            let mut ends = path[2..].match_indices('/').map(|(i, _)| i + 3).skip(1);
            return ends.next().map_or(path, |end| &path[..end]);
        }
        return &path[..1];
    }
    if cfg!(windows) && has_drive_letter(path) {
        return if path[2..].starts_with('/') {
            &path[..3]
        } else {
            &path[..2]
        };
    }
    ""
}

fn has_drive_letter(path: &str) -> bool {
    let mut c = path.chars();
    matches!((c.next(), c.next()), (Some(d), Some(':')) if d.is_ascii_alphabetic())
}

/// Path components, with empty and `.` segments dropped.
///
/// The root is *not* stripped, so `C:/x` yields `["C:", "x"]`. Both sides of
/// every comparison go through this function, so the two agree; a caller that
/// needs the root separately asks [`root_prefix`].
pub fn components(path: &str) -> Vec<&str> {
    path.split(SEPARATORS)
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

/// The last component, or the whole string when there is no separator.
pub fn basename(path: &str) -> &str {
    path.rsplit(SEPARATORS).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_forms_behave_identically_on_every_platform() {
        assert!(is_absolute_str("/etc/passwd"));
        assert!(!is_absolute_str("src/main.rs"));
        assert_eq!(root_prefix("/etc/passwd"), "/");
        assert_eq!(root_prefix("src/main.rs"), "");
        assert_eq!(components("/etc/./passwd"), ["etc", "passwd"]);
        assert_eq!(basename("/etc/passwd"), "passwd");
        assert_eq!(basename("passwd"), "passwd");
    }

    /// A backslash is a filename character on Unix. Folding it there would
    /// invent matches, so the vocabulary must diverge rather than harmonize.
    #[test]
    fn a_backslash_folds_only_where_the_os_folds_it() {
        let folded = normalize_separators(r"a\b");
        if cfg!(windows) {
            assert_eq!(folded, "a/b");
            assert_eq!(basename(r"a\b"), "b");
        } else {
            assert_eq!(folded, r"a\b");
            assert_eq!(basename(r"a\b"), r"a\b");
        }
    }

    #[test]
    #[cfg(windows)]
    fn windows_roots_are_recognized_and_reproducible() {
        for (path, root) in [
            ("C:/src/x", "C:/"),
            ("C:src/x", "C:"),
            ("//srv/share/x", "//srv/share/"),
            ("//?/C:/x", "//?/C:/"),
            (r"\x", "/"),
        ] {
            let n = normalize_separators(path);
            assert_eq!(root_prefix(&n), root, "root of `{path}`");
            assert!(is_absolute_str(&n), "`{path}` must be anchored");
        }
        // A degenerate UNC prefix is anchored too, never relative.
        assert_eq!(root_prefix("//srv"), "//srv");
    }
}
