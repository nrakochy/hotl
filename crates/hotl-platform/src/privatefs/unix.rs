//! `0700`/`0600` at create, via the mode extensions.

use super::{EffectiveAccess, PrivateFs};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

#[derive(Debug, Clone, Copy, Default)]
pub struct UnixPrivateFs;

impl UnixPrivateFs {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for UnixPrivateFs {}

impl PrivateFs for UnixPrivateFs {
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        match DirBuilder::new().mode(0o700).recursive(false).create(path) {
            Ok(()) => Ok(()),
            // `recursive(true)` would swallow a pre-existing loose directory
            // silently; tighten it instead so the postcondition holds either
            // way.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self.harden_existing(path),
            Err(e) => Err(e),
        }
    }

    fn create_file_new(&self, path: &Path) -> io::Result<File> {
        // `create_new` is the `O_EXCL`, `mode` is the at-create restriction —
        // together they leave no window in which the file exists and is
        // readable.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }

    fn harden_existing(&self, path: &Path) -> io::Result<()> {
        let meta = std::fs::metadata(path)?;
        let mode = if meta.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    fn effective_access(&self, path: &Path) -> io::Result<EffectiveAccess> {
        let mode = std::fs::metadata(path)?.mode() & 0o777;
        let group = mode & 0o070;
        let other = mode & 0o007;
        let mut other_readers = Vec::new();
        if group != 0 {
            other_readers.push(format!("group ({:03o})", group >> 3));
        }
        if other != 0 {
            other_readers.push(format!("other ({other:03o})"));
        }
        Ok(EffectiveAccess {
            owner_only: other_readers.is_empty(),
            other_readers,
        })
    }
}
