//! Platform seams (rust-implementation §Workspace layout).
//!
//! M0 carries only what M0 needs: `Clock` and `SecretStore`. Fs/Exec/Http
//! traits join when the browser milestone makes indirection pay for itself;
//! until then native crates use std/tokio/reqwest directly behind their own
//! crate boundaries (the seam is the crate, not yet a trait).

use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Resolution order: env var → SecretStore → prompt.
/// M0 ships the env implementation; Keychain/secret-service land at MD.
pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EnvSecrets;

impl SecretStore for EnvSecrets {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
}
