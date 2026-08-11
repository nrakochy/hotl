//! [`KnownPaths`] — where hotl's home, config, data and runtime directories are.

use std::path::PathBuf;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::UnixKnownPaths;
#[cfg(unix)]
pub type ActiveKnownPaths = UnixKnownPaths;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::WindowsKnownPaths;
#[cfg(windows)]
pub type ActiveKnownPaths = WindowsKnownPaths;

/// The directories hotl reads and writes its own state in.
///
/// CONTRACT: an explicitly-set `XDG_*` or `HOME` wins on **every** platform.
/// That is not Unix bias — it is what keeps a Git Bash or MSYS2 user coherent
/// between their shell and hotl, and those are exactly the users who have a
/// POSIX shell on Windows.
///
/// Every method returns `Option` rather than a fallback path: "no home" must
/// stay expressible, because a missing `$HOME` is how the config layer already
/// decides there is no user rules tier. Narrower, never wider.
pub trait KnownPaths: crate::sealed::Sealed {
    fn home(&self) -> Option<PathBuf>;
    fn config(&self) -> Option<PathBuf>;
    fn data(&self) -> Option<PathBuf>;
    /// A directory for sockets, pipes and pidfiles that need not survive a
    /// reboot. `None` where the platform has no such concept.
    fn runtime(&self) -> Option<PathBuf>;
}

/// Read one env var, rejecting the empty string — an exported-but-empty
/// `XDG_CONFIG_HOME` means "unset", not "the current directory".
pub(crate) fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
pub(crate) fn assert_known_paths_contract<P: KnownPaths>(paths: &P) {
    // The bug this trait exists to prevent, stated as an assertion: a
    // hand-rolled `HOME` lookup returns nothing on Windows, and every caller's
    // fallback chain then lands on a *relative* path — putting hotl's config
    // and its session logs in whatever directory it was launched from. Inside
    // the workspace, inside the sandbox write root, and readable by the agent
    // whose transcripts they are.
    for (what, dir) in [
        ("home", paths.home()),
        ("config", paths.config()),
        ("data", paths.data()),
    ] {
        if let Some(dir) = dir {
            assert!(
                dir.is_absolute(),
                "{what}() must be absolute or None, never a cwd-relative path: {dir:?}"
            );
        }
    }
    if let Some(data) = paths.data() {
        assert!(data.is_absolute(), "data() must be absolute, got {data:?}");
    }
    if let (Some(c), Some(d)) = (paths.config(), paths.data()) {
        assert_ne!(c, d, "config() and data() must not collide");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_adapter_upholds_the_contract() {
        assert_known_paths_contract(&crate::KNOWN_PATHS);
    }

    /// The clause that makes Git Bash coherent with hotl: an explicit
    /// `XDG_DATA_HOME` wins, on Windows too.
    #[test]
    fn an_explicit_xdg_var_wins_on_every_platform() {
        // Serialized against the sibling env test by running in one body — the
        // process env is shared, and a parallel test harness would race.
        let key = "XDG_DATA_HOME";
        let restore = std::env::var_os(key);
        let want = if cfg!(windows) { r"C:\xdg" } else { "/xdg" };
        // SAFETY: single-threaded within this test; restored before returning.
        unsafe { std::env::set_var(key, want) };
        let got = crate::KNOWN_PATHS.data();
        // SAFETY: as above.
        unsafe {
            match restore {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        assert_eq!(got, Some(PathBuf::from(want).join("hotl")));
    }
}
