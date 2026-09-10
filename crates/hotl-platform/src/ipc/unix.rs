//! A `0600` unix-domain socket under the runtime directory.

use super::{Ipc, IpcListener, Liveness, PeerReject};
use crate::KnownPaths as _;
use std::io;
use std::path::PathBuf;

pub type UnixIpcStream = tokio::net::UnixStream;

#[derive(Debug, Clone, Copy, Default)]
pub struct UnixIpc;

impl UnixIpc {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for UnixIpc {}

/// `<runtime>/run`, holding one `<id>.sock` per live session.
fn run_dir() -> PathBuf {
    crate::KNOWN_PATHS
        .data()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("run")
}

/// Where `id`'s socket lives — or would. Unchecked on purpose: the stale-socket
/// sweep needs a path to unlink even for an id that could never be bound.
fn sock_path(id: &str) -> PathBuf {
    run_dir().join(format!("{id}.sock"))
}

/// How much path `sockaddr_un` can carry, read off the struct rather than
/// written down: 103 bytes on macOS, 107 on Linux, the trailing NUL being the
/// one the array holds back.
fn sun_path_max() -> usize {
    // SAFETY: `sockaddr_un` is a plain C struct with no invalid bit patterns,
    // and this one is only ever measured — it never reaches the kernel.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len() - 1
}

/// [`sock_path`], refused up front when it cannot fit in `sun_path`.
///
/// The kernel's own refusal is `InvalidInput: path must be shorter than
/// SUN_LEN` — a C constant, no path, and nothing to do about it. hotl picked
/// this path out of `$XDG_DATA_HOME`/`$HOME`, not the caller, so the refusal
/// owes them the path, the overrun and the knob that moves it. A long username
/// or a deep data dir is enough: with a 78-byte run dir, a 26-character session
/// ULID already blows the macOS cap.
fn addressable_sock_path(id: &str) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = sock_path(id);
    let (len, max) = (path.as_os_str().as_bytes().len(), sun_path_max());
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the session socket path is {len} bytes, {} over the {max} a unix socket \
                 path can hold on this platform: {}\nset XDG_DATA_HOME to a shorter \
                 directory — hotl binds its sockets under <XDG_DATA_HOME>/hotl/run",
                len - max,
                path.display()
            ),
        ));
    }
    Ok(path)
}

pub struct UnixIpcListener(tokio::net::UnixListener);

impl IpcListener for UnixIpcListener {
    type Stream = UnixIpcStream;

    async fn accept(&mut self) -> io::Result<Self::Stream> {
        self.0.accept().await.map(|(s, _)| s)
    }
}

impl Ipc for UnixIpc {
    type Listener = UnixIpcListener;
    type Stream = UnixIpcStream;

    /// A socket file outlives the process that bound it, so the caller must
    /// sweep it — inode-matched, or a stale guard deletes a successor's live
    /// socket.
    const LEAVES_STALE_ARTIFACT: bool = true;

    fn bind_private(&self, id: &str) -> io::Result<Self::Listener> {
        use crate::PrivateFs as _;
        // Ahead of the mkdir: a path that can never be bound should not leave a
        // run directory behind as its only trace.
        let path = addressable_sock_path(id)?;
        let dir = run_dir();
        crate::PRIVATE_FS.create_dir_all(&dir)?;
        // A stale socket from a dead server is cleared; a *live* one is the
        // caller's business to refuse, which is why `liveness` exists.
        if path.exists() && self.liveness(id) == Liveness::Dead {
            let _ = std::fs::remove_file(&path);
        }
        let listener = tokio::net::UnixListener::bind(&path)?;
        // Owner-only: only this uid can connect, even on a shared host. This is
        // the authorization boundary; peer auth is defence in depth.
        crate::PRIVATE_FS.harden_existing(&path)?;
        Ok(UnixIpcListener(listener))
    }

    async fn connect(&self, id: &str) -> io::Result<Self::Stream> {
        tokio::net::UnixStream::connect(addressable_sock_path(id)?).await
    }

    fn authenticate_peer(&self, stream: &Self::Stream) -> Result<(), PeerReject> {
        // `peer_cred` is the portable spelling: `SO_PEERCRED` on Linux,
        // `getpeereid` on BSD/macOS.
        let cred = stream
            .peer_cred()
            .map_err(|e| PeerReject(format!("the peer's credentials are unreadable: {e}")))?;
        // SAFETY: `getuid` takes nothing and cannot fail.
        let me = unsafe { libc::getuid() };
        if cred.uid() != me {
            return Err(PeerReject(format!(
                "the peer runs as uid {}, not {me}",
                cred.uid()
            )));
        }
        Ok(())
    }

    fn liveness(&self, id: &str) -> Liveness {
        // A blocking connect, deliberately: this runs from `gc` and from the
        // bind path, neither of which has a reactor.
        //
        // An unaddressable path is Dead by construction — nothing can be bound
        // there — and `bind_private` is where that gets said out loud.
        let Ok(path) = addressable_sock_path(id) else {
            return Liveness::Dead;
        };
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => Liveness::Live,
            Err(_) => Liveness::Dead,
        }
    }

    fn list_live(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(run_dir()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? != "sock" {
                    return None;
                }
                let id = p.file_stem()?.to_str()?.to_string();
                (self.liveness(&id) == Liveness::Live).then_some(id)
            })
            .collect()
    }

    fn artifact_path(&self, id: &str) -> Option<PathBuf> {
        Some(sock_path(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path too long for `sun_path` is named, measured, and paired with the
    /// knob that shortens it. The kernel says `SUN_LEN`, which names a constant
    /// the caller has never heard of on a path they never chose.
    #[test]
    fn an_over_long_socket_path_is_refused_by_name() {
        // `run_dir()` reads `data()`, which a sibling test rewrites through
        // `XDG_DATA_HOME`; hold the lock that one holds.
        let _env = crate::ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let id = "x".repeat(200);
        let err = addressable_sock_path(&id).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        for want in [
            sock_path(&id).display().to_string(),
            sun_path_max().to_string(),
            "XDG_DATA_HOME".to_string(),
        ] {
            assert!(msg.contains(&want), "the refusal must name `{want}`: {msg}");
        }
        // Positive control, so the check cannot degenerate into "always
        // refuse". One character, because the suite's own run dir is long
        // under `nix flake check` — which is the bug this guard reports.
        assert!(addressable_sock_path("s").is_ok());
    }
}
