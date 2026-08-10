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

fn sock_path(id: &str) -> PathBuf {
    run_dir().join(format!("{id}.sock"))
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
        let dir = run_dir();
        crate::PRIVATE_FS.create_dir_all(&dir)?;
        let path = sock_path(id);
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
        tokio::net::UnixStream::connect(sock_path(id)).await
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
        match std::os::unix::net::UnixStream::connect(sock_path(id)) {
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
