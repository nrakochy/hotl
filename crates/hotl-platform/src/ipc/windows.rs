//! A named pipe whose DACL names only the current user.
//!
//! Two things here have **no Unix-socket counterpart**, and forgetting either
//! is a bug no ported test would catch, because there was nothing to port:
//!
//! 1. **`reject_remote_clients`.** Named pipes are reachable over SMB. Tokio
//!    defaults it on; this asserts it rather than inheriting it, because the
//!    failure mode is remote exposure.
//! 2. **`ERROR_PIPE_BUSY` means the server is ALIVE.** It says a server exists
//!    and every instance is currently busy. Reading it as "dead" makes hotl
//!    steal a running session's pipe name.

use super::{Ipc, IpcListener, Liveness, PeerReject};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::ptr;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, HANDLE};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, RevertToSelf, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};

pub type WindowsIpcStream = NamedPipeServerOrClient;

#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsIpc;

impl WindowsIpc {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for WindowsIpc {}

/// `\\.\pipe\hotl-<id>`. The pipe namespace is flat and machine-wide, so the
/// name carries the prefix that keeps two hotl installs from colliding; the
/// DACL, not the name, is what keeps another *user* out.
fn pipe_name(id: &str) -> String {
    format!(r"\\.\pipe\hotl-{id}")
}

const PREFIX: &str = "hotl-";

/// One end of a connected pipe, whichever side we are.
///
/// Both sides implement `AsyncRead + AsyncWrite` identically, and the session
/// protocol is symmetric, so a single type keeps `Ipc::Stream` to one shape the
/// way a `UnixStream` is one shape on the other platform.
pub enum NamedPipeServerOrClient {
    Server(NamedPipeServer),
    Client(tokio::net::windows::named_pipe::NamedPipeClient),
}

macro_rules! delegate {
    ($self:ident, $inner:ident, $body:expr) => {
        match $self.get_mut() {
            NamedPipeServerOrClient::Server($inner) => $body,
            NamedPipeServerOrClient::Client($inner) => $body,
        }
    };
}

impl tokio::io::AsyncRead for NamedPipeServerOrClient {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        delegate!(self, s, std::pin::Pin::new(s).poll_read(cx, buf))
    }
}

impl tokio::io::AsyncWrite for NamedPipeServerOrClient {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        delegate!(self, s, std::pin::Pin::new(s).poll_write(cx, buf))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        delegate!(self, s, std::pin::Pin::new(s).poll_flush(cx))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        delegate!(self, s, std::pin::Pin::new(s).poll_shutdown(cx))
    }
}

/// Holds the *next* server instance, because a named pipe accepts by connecting
/// the instance it already created and then creating a successor. Without the
/// successor there is a window in which the pipe name does not exist and a
/// client gets `ERROR_FILE_NOT_FOUND` from a live server.
pub struct WindowsIpcListener {
    name: String,
    next: Option<NamedPipeServer>,
}

impl IpcListener for WindowsIpcListener {
    type Stream = WindowsIpcStream;

    async fn accept(&mut self) -> io::Result<Self::Stream> {
        let server = match self.next.take() {
            Some(s) => s,
            None => new_server_instance(&self.name, false)?,
        };
        server.connect().await?;
        // Create the successor *before* handing this one over, so the name is
        // continuously available.
        self.next = Some(new_server_instance(&self.name, false)?);
        Ok(NamedPipeServerOrClient::Server(server))
    }
}

/// A pipe instance whose DACL grants only the current user.
///
/// `first` marks the instance that claims the name: `ServerOptions::first_pipe_instance`
/// makes a second server on the same name fail rather than silently join, which
/// is what keeps two hotl sessions from sharing one endpoint.
fn new_server_instance(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first)
        // Asserted, not inherited: a named pipe is reachable over SMB and this
        // is the only thing that stops it. There is no Unix-socket analogue, so
        // no ported test would have caught its absence.
        .reject_remote_clients(true);
    let mut attrs = crate::privatefs::owner_only_attributes()?;
    // SAFETY: `attrs` outlives the call, and `CreateNamedPipeW` copies the
    // descriptor into the object it creates rather than retaining the pointer.
    unsafe { opts.create_with_security_attributes_raw(name, attrs.as_ptr()) }
}

impl Ipc for WindowsIpc {
    type Listener = WindowsIpcListener;
    type Stream = WindowsIpcStream;

    /// A pipe is a kernel object that vanishes with its last handle. There is
    /// nothing to sweep, so the caller's unlink guard becomes a documented
    /// no-op rather than dead machinery ported for symmetry — strictly better
    /// than the Unix behavior, and the const is what says so.
    const LEAVES_STALE_ARTIFACT: bool = false;

    fn bind_private(&self, id: &str) -> io::Result<Self::Listener> {
        let name = pipe_name(id);
        let first = new_server_instance(&name, true)?;
        Ok(WindowsIpcListener {
            name,
            next: Some(first),
        })
    }

    async fn connect(&self, id: &str) -> io::Result<Self::Stream> {
        // `SECURITY_IDENTIFICATION` lets the server impersonate us far enough
        // to read our SID and no further. We control both ends, so this is the
        // narrowest level that still allows `authenticate_peer` to work.
        const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
        let name = pipe_name(id);
        // A live server can have every instance momentarily busy between accept
        // iterations; Win32 returns ERROR_PIPE_BUSY, which the docs say to wait
        // out and retry (the same fact `liveness` already trusts). Bounded so a
        // wedged server surfaces the busy error rather than hanging forever.
        let client = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match ClientOptions::new()
                    .security_qos_flags(SECURITY_IDENTIFICATION)
                    .open(&name)
                {
                    Ok(c) => return Ok::<_, io::Error>(c),
                    Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the named pipe stayed busy"))??;
        Ok(NamedPipeServerOrClient::Client(client))
    }

    fn authenticate_peer(&self, stream: &Self::Stream) -> Result<(), PeerReject> {
        use std::os::windows::io::AsRawHandle;
        let NamedPipeServerOrClient::Server(server) = stream else {
            // Only a server can impersonate a client. A client asking who it is
            // talking to has the DACL and nothing else, which is the boundary
            // anyway — say so rather than pretend to check.
            return Ok(());
        };
        let handle = server.as_raw_handle() as HANDLE;
        // SAFETY: a pipe handle we own with a client connected.
        if unsafe { ImpersonateNamedPipeClient(handle) } == 0 {
            return Err(PeerReject(format!(
                "could not impersonate the peer: {}",
                io::Error::last_os_error()
            )));
        }
        let peer = token_user(true);
        // SAFETY: paired with the impersonation above, on every path.
        unsafe { RevertToSelf() };
        let peer = peer.map_err(|e| PeerReject(format!("the peer's token is unreadable: {e}")))?;
        let me = token_user(false)
            .map_err(|e| PeerReject(format!("our own token is unreadable: {e}")))?;
        // SAFETY: both buffers hold a `TOKEN_USER` the OS wrote.
        let same = unsafe { EqualSid(sid_of(&peer), sid_of(&me)) } != 0;
        if !same {
            return Err(PeerReject("the peer runs as a different user".to_string()));
        }
        Ok(())
    }

    fn liveness(&self, id: &str) -> Liveness {
        match ClientOptions::new().open(pipe_name(id)) {
            Ok(_) => Liveness::Live,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                // A server exists and every instance is busy. This is the arm
                // that inverts if you read it as "dead".
                Liveness::Live
            }
            Err(_) => Liveness::Dead,
        }
    }

    fn list_live(&self) -> Vec<String> {
        // The pipe namespace really is enumerable.
        let pattern: Vec<u16> = OsStr::new(r"\\.\pipe\*")
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: zeroed is a valid instance; the call fills it in.
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        // SAFETY: a NUL-terminated pattern and a live out-param.
        let find = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
        if find.is_null() || find as isize == -1 {
            return Vec::new();
        }
        let mut out = Vec::new();
        loop {
            let len = data
                .cFileName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(data.cFileName.len());
            let name = OsString::from_wide(&data.cFileName[..len]);
            if let Some(id) = name.to_string_lossy().strip_prefix(PREFIX) {
                out.push(id.to_string());
            }
            // SAFETY: a live find handle and out-param.
            if unsafe { FindNextFileW(find, &mut data) } == 0 {
                break;
            }
        }
        // SAFETY: a find handle we opened.
        unsafe { FindClose(find) };
        out
    }

    fn artifact_path(&self, _id: &str) -> Option<PathBuf> {
        // Nothing on disk to unlink — see `LEAVES_STALE_ARTIFACT`.
        None
    }
}

/// The `TOKEN_USER` blob for the current thread's (impersonated) or process's
/// token.
fn token_user(thread: bool) -> io::Result<Vec<u8>> {
    let mut token: HANDLE = ptr::null_mut();
    let opened = if thread {
        // SAFETY: our own thread, with the impersonation token in place.
        unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) }
    } else {
        // SAFETY: a pseudo-handle for our own process.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
    };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut needed = 0u32;
    // SAFETY: the deliberate zero-length probe that reports the size.
    unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is exactly the size the OS just asked for.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    // SAFETY: a handle we opened and no longer need, on both paths.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

fn sid_of(buf: &[u8]) -> PSID {
    // SAFETY: the buffer holds a `TOKEN_USER` whose first field is the SID
    // pointer the OS wrote.
    unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid }
}
