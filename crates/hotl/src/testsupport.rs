//! Cross-platform primitives the containment tests need.
//!
//! Symlinks are the whole point: the guard's job is refusing them, so a suite
//! that could only *create* them on Unix would leave the Windows reparse-point
//! refusals — the ones with the junction trap in them — untested. This makes
//! the same test body run on both.

use std::path::Path;

/// Create a symlink, or return `false` if this platform will not let us.
///
/// Windows needs Developer Mode or `SeCreateSymbolicLinkPrivilege`, so a stock
/// developer machine may refuse. A test that gets `false` must **skip**, not
/// fail: the refusal says nothing about the code under test. Callers use
/// [`symlink_or_skip`], which makes that explicit at the call site.
pub(crate) fn try_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        // Windows takes the link kind up front and gets it wrong in a way that
        // matters: a file symlink pointing at a directory does not resolve.
        let is_dir = std::fs::metadata(target).is_ok_and(|m| m.is_dir());
        if is_dir {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        } else {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }
}

/// `try_symlink`, with the skip spelled out.
///
/// Returns `false` when the test should return early. The macro form would hide
/// which tests can silently do nothing; this way `grep` finds them.
macro_rules! symlink_or_skip {
    ($target:expr, $link:expr) => {
        if !crate::testsupport::try_symlink($target.as_ref(), $link.as_ref()) {
            // No symlink privilege (stock Windows without Developer Mode).
            // The guard's refusal is what is under test, and we cannot build
            // the thing to refuse.
            return;
        }
    };
}

pub(crate) use symlink_or_skip;
