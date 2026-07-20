//! Server trust store (SECURITY.md §M3a first-use screen): approval is
//! recorded per server as the SHA-256 of its binary; a changed binary
//! re-raises the protected ask (content-hash revocation).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub struct TrustStore {
    path: PathBuf,
    /// server name → approved binary hash
    approved: HashMap<String, String>,
}

impl TrustStore {
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join("trust.toml");
        let approved = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| toml::from_str::<HashMap<String, String>>(&raw).ok())
            .unwrap_or_default();
        Self { path, approved }
    }

    pub fn is_trusted(&self, server: &str, hash: &str) -> bool {
        self.approved.get(server).is_some_and(|h| h == hash)
    }

    /// Record approval durably; a write failure keeps the in-memory grant
    /// (the session proceeds) but the next session will re-ask — fail-open
    /// on convenience, fail-closed on trust.
    pub fn record(&mut self, server: &str, hash: &str) {
        self.approved.insert(server.to_string(), hash.to_string());
        if let Ok(raw) = toml::to_string(&self.approved) {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&self.path, raw);
        }
    }
}

/// SHA-256 of the server binary, resolving bare names through PATH the same
/// way the shell will. Unreadable binaries hash as `unavailable:` — still
/// recorded, so the screen shows honestly that no integrity check applies.
pub fn binary_hash(command: &str) -> String {
    let path = resolve(command);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            format!("sha256:{:x}", hasher.finalize())
        }
        Err(e) => format!("unavailable:{e}"),
    }
}

fn resolve(command: &str) -> PathBuf {
    let direct = PathBuf::from(command);
    if command.contains('/') || direct.is_file() {
        return direct;
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(command);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    direct
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_roundtrip_and_hash_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = TrustStore::load(dir.path());
        assert!(!store.is_trusted("docs", "sha256:aa"));
        store.record("docs", "sha256:aa");
        assert!(store.is_trusted("docs", "sha256:aa"));
        // Reload from disk: durable.
        let store2 = TrustStore::load(dir.path());
        assert!(store2.is_trusted("docs", "sha256:aa"));
        // A different hash (binary changed) is NOT trusted.
        assert!(!store2.is_trusted("docs", "sha256:bb"));
    }

    #[test]
    fn hashes_real_files_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("server");
        std::fs::write(&bin, b"#!/bin/sh\necho hi").unwrap();
        let h = binary_hash(bin.to_str().unwrap());
        assert!(h.starts_with("sha256:"));
        assert_eq!(h, binary_hash(bin.to_str().unwrap()), "deterministic");
        assert!(binary_hash("/definitely/not/here").starts_with("unavailable:"));
    }
}
