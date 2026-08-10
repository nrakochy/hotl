//! Platform seams: one capability trait per concern, one adapter per platform.
//!
//! `ARCHITECTURE.md` says core crates sit behind platform traits so the seam
//! stays clean. This crate is that seam. A second OS is what makes the
//! abstraction pay for itself, and the native Windows port (plan 0027) is what
//! built it out from the original `Clock` + `SecretStore` stub.
//!
//! # The rules every adapter here follows
//!
//! 1. **One adapter per capability per platform**, named `<Platform><Capability>`,
//!    living in `src/<capability>/{mod,unix,windows}.rs`. `mod.rs` holds the
//!    trait and everything platform-free.
//! 2. **Static dispatch.** Each capability exports a `#[cfg]`-selected type
//!    alias and a unit const, so a call site writes
//!    `hotl_platform::PRIVATE_FS.create_dir(p)?` and pays nothing. There is
//!    exactly one implementation in any build; the point is that the contract
//!    is named, documented and testable, not that it is swappable at runtime.
//!    The one `dyn` in this crate is [`SecretStore`], whose set genuinely is
//!    heterogeneous at runtime (env → keychain → prompt).
//! 3. **Traits are sealed** — see [`sealed`].
//! 4. **Capability-narrow by construction.** Before adding a method, ask what
//!    it would let a caller do that the module exists to forbid. A general
//!    `Fs` trait with `open(&Path)` would demote `fsguard`'s structural
//!    one-door invariant to a discipline invariant; [`DirHandle`] instead
//!    exposes only relative-to-handle, one-component-at-a-time operations.
//! 5. **Totality: no silent no-ops.** Where a platform genuinely lacks a
//!    capability the method returns [`Unsupported`], never `Ok(())`.
//! 6. **Adapters are thin, and policy never lives in one.** An adapter
//!    translates one contract into one platform's syscalls. The moment it
//!    makes a *decision*, two platforms have begun to diverge silently.
//! 7. **The doc comment carries the contract, including what differs.**
//! 8. **Parity tests are generic over the trait**, instantiated on the active
//!    adapter, so one test body runs on every OS.
//! 9. **Test adapters live behind the `testing` feature**, never the default
//!    build.
//! 10. **Adding a platform means implementing the traits, not editing call
//!     sites.** If a future WASM or arm64-Windows port has to touch anything
//!     outside `src/*/`, the seam leaked. That is the acceptance test.

use std::time::{SystemTime, UNIX_EPOCH};

pub mod entropy;
pub mod openat;
pub mod paths;
pub mod privatefs;
pub mod sealed;

pub use entropy::{ActiveEntropy, Entropy};
pub use openat::{ActiveDirHandle, DirHandle, Excl, GuardIo, NodeId, NodeKind, OpenMode};
pub use paths::{ActiveKnownPaths, KnownPaths};
pub use privatefs::{ActivePrivateFs, EffectiveAccess, PrivateFs, Writes};

/// The active adapters. Call sites use these rather than naming a platform
/// type, which is what keeps rule 10 checkable.
pub const PRIVATE_FS: ActivePrivateFs = ActivePrivateFs::new();
pub const KNOWN_PATHS: ActiveKnownPaths = ActiveKnownPaths::new();
pub const ENTROPY: ActiveEntropy = ActiveEntropy::new();

/// A capability this platform does not have.
///
/// Adapters return this rather than quietly succeeding: a no-op adapter is
/// exactly how a security control becomes a rubber stamp, and it reads as
/// "working" in every test that only checks the `Result`. `because` is rendered
/// by `hotl doctor`, so write it for a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub capability: &'static str,
    pub platform: &'static str,
    pub because: &'static str,
}

impl Unsupported {
    pub const fn new(capability: &'static str, because: &'static str) -> Self {
        Self {
            capability,
            platform: std::env::consts::OS,
            because,
        }
    }
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is not available on {}: {}",
            self.capability, self.platform, self.because
        )
    }
}

impl std::error::Error for Unsupported {}

impl From<Unsupported> for std::io::Error {
    fn from(u: Unsupported) -> Self {
        std::io::Error::new(std::io::ErrorKind::Unsupported, u.to_string())
    }
}

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
///
/// The one deliberately `dyn` seam in this crate (rule 2): the implementations
/// really are chosen at runtime and really are heterogeneous.
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
