//! XDG, with the `$HOME`-relative defaults the spec prescribes.

use super::{env_path, KnownPaths};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default)]
pub struct UnixKnownPaths;

impl UnixKnownPaths {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for UnixKnownPaths {}

impl KnownPaths for UnixKnownPaths {
    fn home(&self) -> Option<PathBuf> {
        env_path("HOME")
    }

    fn config(&self) -> Option<PathBuf> {
        env_path("XDG_CONFIG_HOME")
            .or_else(|| self.home().map(|h| h.join(".config")))
            .map(|b| b.join("hotl"))
    }

    fn data(&self) -> Option<PathBuf> {
        env_path("XDG_DATA_HOME")
            .or_else(|| self.home().map(|h| h.join(".local/share")))
            .map(|b| b.join("hotl"))
    }

    fn runtime(&self) -> Option<PathBuf> {
        env_path("XDG_RUNTIME_DIR").map(|b| b.join("hotl"))
    }
}
