//! [`Ipc`] — a private, same-user, local-only session endpoint.

use std::future::Future;
use std::io;
use std::path::PathBuf;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{UnixIpc, UnixIpcListener, UnixIpcStream};
#[cfg(unix)]
pub type ActiveIpc = UnixIpc;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{WindowsIpc, WindowsIpcListener, WindowsIpcStream};
#[cfg(windows)]
pub type ActiveIpc = WindowsIpc;

/// A private, same-user, local-only session endpoint.
///
/// CONTRACT: [`bind_private`](Ipc::bind_private) returns an endpoint reachable
/// **only** by the current user, and that restriction is applied at bind, not
/// after. The authorization boundary is the OS object's own access control —
/// the `0600` mode on Unix, the DACL on Windows.
/// [`authenticate_peer`](Ipc::authenticate_peer) is defence in depth and is
/// never the only check.
pub trait Ipc: crate::sealed::Sealed {
    type Listener: IpcListener<Stream = Self::Stream>;
    type Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static;

    /// Whether a dead server leaves an artifact that must be swept.
    ///
    /// Unix: `true` — a socket file, hence the caller's unlink guard. Windows:
    /// `false`, because a named pipe is a kernel object that vanishes with its
    /// last handle. The const says so, so the guard is a documented no-op
    /// rather than dead machinery ported for symmetry.
    const LEAVES_STALE_ARTIFACT: bool;

    fn bind_private(&self, id: &str) -> io::Result<Self::Listener>;
    fn connect(&self, id: &str) -> impl Future<Output = io::Result<Self::Stream>> + Send;

    /// Reject a peer that is not the current user.
    ///
    /// Unix: `SO_PEERCRED` via `peer_cred()`. Windows:
    /// `ImpersonateNamedPipeClient` + `EqualSid` + `RevertToSelf`, which is the
    /// correct analogue because it captures the client's token *as of the
    /// connection*.
    ///
    /// **Not `GetNamedPipeClientProcessId` + `OpenProcess`.** That is the
    /// obvious-looking answer and it is racy — the pid can be reused between
    /// the connect and the lookup, where `SO_PEERCRED` cannot. Written down
    /// because someone will propose it.
    fn authenticate_peer(&self, stream: &Self::Stream) -> Result<(), PeerReject>;

    /// Is a server listening on `id`?
    ///
    /// Tri-state on purpose. A two-state bool is how the Windows port ships a
    /// bug: `ERROR_PIPE_BUSY` means a server **exists** and all its instances
    /// are busy — i.e. LIVE. Reading it as dead makes hotl steal a running
    /// session's pipe name.
    fn liveness(&self, id: &str) -> Liveness;

    fn list_live(&self) -> Vec<String>;

    /// The on-disk artifact for `id`, when the platform has one. `None` on
    /// Windows, where there is nothing to unlink.
    fn artifact_path(&self, id: &str) -> Option<PathBuf>;
}

/// Accepting is a method on the listener rather than the adapter, because a
/// named-pipe server must create the *next* instance as part of accepting the
/// current one — there is state to carry that a stateless call cannot.
pub trait IpcListener: Send {
    type Stream;
    fn accept(&mut self) -> impl Future<Output = io::Result<Self::Stream>> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Dead,
}

#[derive(Debug)]
pub struct PeerReject(pub String);

impl std::fmt::Display for PeerReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// One body, both transports: bind, connect, authenticate, round-trip a
    /// frame, and see the endpoint go from live to dead.
    #[test]
    fn active_adapter_upholds_the_contract() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ipc = crate::IPC;
            let id = format!("hotl-ipc-test-{}", std::process::id());
            assert_eq!(ipc.liveness(&id), Liveness::Dead, "nothing bound yet");

            let mut listener = ipc.bind_private(&id).unwrap();
            assert_eq!(ipc.liveness(&id), Liveness::Live);
            assert!(ipc.list_live().contains(&id), "a bound endpoint is listed");

            // Accept in a loop, skipping peers that fail authentication —
            // which is what a real server does, and what it must do here: the
            // `liveness` probe above connected and dropped, leaving a dead
            // connection queued in the backlog ahead of the real client.
            let server = tokio::spawn(async move {
                let stream = loop {
                    let s = listener.accept().await.unwrap();
                    if crate::IPC.authenticate_peer(&s).is_ok() {
                        break s;
                    }
                };
                let (r, mut w) = tokio::io::split(stream);
                let mut lines = BufReader::new(r).lines();
                let got = lines.next_line().await.unwrap().unwrap();
                w.write_all(format!("echo:{got}\n").as_bytes())
                    .await
                    .unwrap();
                w.flush().await.unwrap();
            });

            let client = ipc.connect(&id).await.unwrap();
            let (r, mut w) = tokio::io::split(client);
            w.write_all(b"ping\n").await.unwrap();
            w.flush().await.unwrap();
            let mut lines = BufReader::new(r).lines();
            assert_eq!(lines.next_line().await.unwrap().unwrap(), "echo:ping");
            server.await.unwrap();

            // An adapter that names an artifact must admit it leaves one, and
            // vice versa — the const and the path have to agree or a caller's
            // sweep is either dead code or a missing cleanup.
            assert_eq!(
                ipc.artifact_path(&id).is_some(),
                <ActiveIpc as Ipc>::LEAVES_STALE_ARTIFACT
            );
            if let Some(p) = ipc.artifact_path(&id) {
                let _ = std::fs::remove_file(p);
            }
        });
    }
}
