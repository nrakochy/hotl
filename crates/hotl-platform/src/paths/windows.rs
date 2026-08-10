//! `%LOCALAPPDATA%`, with the XDG vars still winning where they are set.

use super::{env_path, KnownPaths};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsKnownPaths;

impl WindowsKnownPaths {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for WindowsKnownPaths {}

impl KnownPaths for WindowsKnownPaths {
    /// `HOME` → `USERPROFILE` → `FOLDERID_Profile`.
    ///
    /// `HOME` first is not a Unix habit leaking in: a Git Bash session sets it,
    /// and hotl's credential set is `$HOME`-relative, so disagreeing with the
    /// shell would classify `~/.ssh` under one root while the shell writes it
    /// under another.
    fn home(&self) -> Option<PathBuf> {
        env_path("HOME")
            .or_else(|| env_path("USERPROFILE"))
            .or_else(dirs::home_dir)
    }

    /// An explicit `XDG_CONFIG_HOME` gets XDG's own layout, so a Git Bash user
    /// who set it sees the same tree hotl uses on their Linux box. Only the
    /// `%LOCALAPPDATA%` fallback needs the extra segment, because there config
    /// and data share one base.
    fn config(&self) -> Option<PathBuf> {
        env_path("XDG_CONFIG_HOME")
            .map(|b| b.join("hotl"))
            .or_else(|| local_app_data().map(|b| b.join("hotl").join("config")))
    }

    fn data(&self) -> Option<PathBuf> {
        env_path("XDG_DATA_HOME")
            .map(|b| b.join("hotl"))
            .or_else(|| local_app_data().map(|b| b.join("hotl").join("data")))
    }

    /// Windows has no `XDG_RUNTIME_DIR` analogue, and it needs none: the
    /// session endpoint is a named pipe, which is a kernel object with no
    /// filesystem artifact to place. `None` here is a fact about the platform,
    /// not a gap — see `Ipc::LEAVES_STALE_ARTIFACT`.
    fn runtime(&self) -> Option<PathBuf> {
        env_path("XDG_RUNTIME_DIR").map(|b| b.join("hotl"))
    }
}

/// **Local**, never roaming `%APPDATA%`. `config.toml` carries machine-specific
/// absolute paths in `[sandbox].writable`; roaming them to a second machine
/// would silently widen or break the write floor there.
fn local_app_data() -> Option<PathBuf> {
    env_path("LOCALAPPDATA").or_else(dirs::data_local_dir)
}
