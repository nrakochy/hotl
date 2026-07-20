//! Network-egress policy for `bash` (SECURITY.md "Network egress").
//!
//! Three modes, configured in `[network]` of `~/.config/hotl/config.toml`:
//! - **Open** (default) — today's behavior: egress unrestricted, the human
//!   gate is the exfiltration boundary.
//! - **Off** — the kernel confines the command to loopback and unix-domain
//!   sockets; no egress.
//! - **Allowlist** — the same kernel loopback-only confinement, plus a local
//!   filtering HTTP proxy for the listed hosts. Cooperating clients (anything
//!   honoring `HTTP(S)_PROXY`) reach allowed hosts through the proxy;
//!   non-cooperating clients fail closed at the kernel wall.
//!
//! The policy is process-wide, installed once at startup (mirroring
//! `sandbox_status()`); child sessions inherit it via the global. When the
//! kernel side can't back a configured restriction (no seatbelt, Landlock
//! without the net ABI, `HOTL_SANDBOX=off`) the state degrades **fail-closed**
//! to `Unenforced`: asks are loudly marked and bash allow-rules stop
//! auto-approving — the same posture as UNSANDBOXED.

use std::sync::OnceLock;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::sandbox::SandboxStatus;

/// Cap on a proxied request head; a client that can't fit its request line
/// and headers in this is malformed (or hostile).
const MAX_HEAD: usize = 16 * 1024;

/// The configured policy (what the owner asked for).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressPolicy {
    /// Egress unrestricted (the default).
    Open,
    /// Loopback + unix sockets only.
    Off,
    /// Loopback + unix sockets at the kernel, plus the filtering proxy for
    /// these host patterns (`"github.com"`, `"*.crates.io"`).
    Allowlist(Vec<String>),
}

static POLICY: OnceLock<EgressPolicy> = OnceLock::new();

/// Install the process-wide policy, once, at startup. Later calls are no-ops
/// (set-once), so nothing downstream can widen the policy back to Open —
/// child sessions inherit whatever the process started with.
pub fn init(policy: EgressPolicy) {
    let _ = POLICY.set(policy);
}

fn policy() -> &'static EgressPolicy {
    POLICY.get().unwrap_or(&EgressPolicy::Open)
}

/// The resolved runtime state (what the host can actually enforce).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressState {
    Open,
    Off,
    /// Allowlist active; the filtering proxy listens on 127.0.0.1 at this port.
    Proxy(u16),
    /// A restriction is configured but the kernel can't back it. Fail-closed
    /// consequences: loud ask marker, bash allow-rules stop auto-approving.
    Unenforced(String),
}

/// Resolve the policy against this host. For Allowlist the proxy is started
/// lazily, once; Off/Open pass through.
pub async fn egress_state() -> EgressState {
    let policy = policy();
    if matches!(policy, EgressPolicy::Open) {
        return EgressState::Open;
    }
    if let Err(reason) = kernel_backing(crate::builtins::sandbox_status()) {
        return EgressState::Unenforced(reason);
    }
    match policy {
        EgressPolicy::Open => EgressState::Open,
        EgressPolicy::Off => EgressState::Off,
        EgressPolicy::Allowlist(patterns) => match proxy_port(patterns).await {
            Some(port) => EgressState::Proxy(port),
            None => EgressState::Unenforced("the egress filtering proxy failed to start".into()),
        },
    }
}

/// Can the kernel back a network restriction on this host? The proxy alone is
/// never enough — only cooperating clients honor it; the kernel wall is what
/// makes the restriction real.
fn kernel_backing(status: &SandboxStatus) -> Result<(), String> {
    match status {
        SandboxStatus::Enforced("seatbelt") => Ok(()),
        #[cfg(target_os = "linux")]
        SandboxStatus::Enforced("landlock") => crate::sandbox::landlock_net_supported(),
        SandboxStatus::Enforced(m) => Err(format!("`{m}` cannot confine the network")),
        SandboxStatus::Unavailable(r) => Err(format!("no sandbox floor: {r}")),
        SandboxStatus::Disabled => Err("HOTL_SANDBOX=off".into()),
    }
}

/// Whether bash allow-rules may auto-approve. Auto-approval requires the
/// egress posture to be honest: policy Open (nothing promised), or a
/// restriction the kernel actually enforces. Mirrors the "bash rules need the
/// floor" carve-out in `rules.rs`.
pub fn auto_allow_permitted(status: &SandboxStatus) -> bool {
    matches!(policy(), EgressPolicy::Open) || kernel_backing(status).is_ok()
}

/// The egress marker for the bash ask label; `None` when the policy is Open
/// (the label stays exactly as it was).
pub fn label_suffix() -> Option<String> {
    let label = match policy() {
        EgressPolicy::Open => return None,
        EgressPolicy::Off => "net:off".to_string(),
        EgressPolicy::Allowlist(patterns) => format!("net:allow({})", patterns.len()),
    };
    match kernel_backing(crate::builtins::sandbox_status()) {
        Ok(()) => Some(label),
        Err(reason) => Some(format!("NET:UNENFORCED({reason})")),
    }
}

/// Host-pattern match, case-insensitive. Exact match, or `*.example.com`
/// which matches the apex (`example.com`) **and** any subdomain depth
/// (`a.example.com`, `a.b.example.com`). No ports in patterns; a trailing dot
/// on the host is stripped. An empty pattern list allows nothing.
fn host_allowed(host: &str, patterns: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    patterns.iter().any(|pattern| {
        let pattern = pattern.to_ascii_lowercase();
        match pattern.strip_prefix("*.") {
            Some(apex) => host == apex || host.ends_with(&format!(".{apex}")),
            None => host == pattern,
        }
    })
}

/// Lazily start the proxy (once per process) and return its port; `None` if
/// the listener could not bind.
async fn proxy_port(patterns: &'static [String]) -> Option<u16> {
    static PROXY: tokio::sync::OnceCell<Option<u16>> = tokio::sync::OnceCell::const_new();
    *PROXY.get_or_init(|| start_proxy(patterns)).await
}

/// Bind 127.0.0.1:0 and serve connections forever, one task each. No global
/// state beyond the listener; the allowlist is the static policy.
async fn start_proxy(patterns: &'static [String]) -> Option<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let port = listener.local_addr().ok()?.port();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(handle_conn(stream, patterns));
                }
                // Transient accept failure (fd pressure): back off, keep serving.
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
    });
    Some(port)
}

/// One proxied connection: read the request head, check the target host,
/// tunnel or deny. The 403 body is an errors-as-prompts message — the model
/// sees it in tool output and learns which control blocked it.
async fn handle_conn(mut client: TcpStream, patterns: &'static [String]) {
    // Read until the end of the head (CRLFCRLF), capped.
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let head_end = loop {
        if let Some(end) = find_head_end(&buf) {
            break end;
        }
        if buf.len() >= MAX_HEAD {
            return respond(&mut client, "400 Bad Request", "request head too large").await;
        }
        let mut chunk = [0u8; 4096];
        match client.read(&mut chunk).await {
            Ok(0) | Err(_) => return, // client went away mid-head
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut parts = head.lines().next().unwrap_or("").split_ascii_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    if method == "CONNECT" {
        // CONNECT host:port — establish a blind tunnel (TLS goes through here).
        let Some((host, port)) = split_host_port(target, None) else {
            return respond(&mut client, "400 Bad Request", "malformed CONNECT target").await;
        };
        if !host_allowed(&host, patterns) {
            return deny_host(&mut client, &host).await;
        }
        let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)).await else {
            return respond(&mut client, "502 Bad Gateway", "upstream connect failed").await;
        };
        if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return;
        }
        // Bytes the client pipelined past the head belong to the tunnel.
        tunnel(&mut client, &mut upstream, &buf[head_end..]).await;
        return;
    }

    // Absolute-form plain HTTP (`GET http://host/path`), Host header fallback.
    let Some((host, port)) = http_target(target, &head) else {
        return respond(&mut client, "400 Bad Request", "no target host in request").await;
    };
    if !host_allowed(&host, patterns) {
        return deny_host(&mut client, &host).await;
    }
    let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)).await else {
        return respond(&mut client, "502 Bad Gateway", "upstream connect failed").await;
    };
    // Forward everything already read (head + any pipelined bytes), then relay.
    tunnel(&mut client, &mut upstream, &buf).await;
}

async fn deny_host(client: &mut TcpStream, host: &str) {
    let body = format!("hotl egress: \"{host}\" is not in [network].allow");
    respond(client, "403 Forbidden", &body).await;
}

async fn respond(client: &mut TcpStream, status: &str, body: &str) {
    let reply = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(reply.as_bytes()).await;
    let _ = client.shutdown().await;
}

/// Send `prelude` upstream, then relay both directions until either side closes.
async fn tunnel(client: &mut TcpStream, upstream: &mut TcpStream, prelude: &[u8]) {
    if !prelude.is_empty() && upstream.write_all(prelude).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(client, upstream).await;
}

/// Index just past the head terminator (`\r\n\r\n`), if present.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// The (host, port) of a non-CONNECT request: absolute-form URI first, the
/// Host header as fallback. Default port 80.
fn http_target(target: &str, head: &str) -> Option<(String, u16)> {
    let authority = match target.strip_prefix("http://") {
        Some(rest) => rest.split(['/', '?']).next().unwrap_or("").to_string(),
        None => host_header(head)?,
    };
    split_host_port(&authority, Some(80))
}

fn host_header(head: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("host").then(|| value.trim().to_string())
    })
}

/// Split `host[:port]` (brackets tolerated for IPv6 literals). With no
/// explicit port, `default` applies — `None` means a port is required.
fn split_host_port(authority: &str, default: Option<u16>) -> Option<(String, u16)> {
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        let port = match rest.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default?,
        };
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            Some((host.to_string(), port.parse().ok()?))
        }
        _ => Some((authority.to_string(), default?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn host_allowed_matrix() {
        let p = patterns(&["github.com", "*.crates.io"]);
        // Exact.
        assert!(host_allowed("github.com", &p));
        assert!(!host_allowed("api.github.com", &p)); // exact is not a wildcard
        // Wildcard covers the apex and every subdomain depth.
        assert!(host_allowed("crates.io", &p));
        assert!(host_allowed("static.crates.io", &p));
        assert!(host_allowed("a.b.crates.io", &p));
        // Case-insensitive both sides; trailing dot stripped.
        assert!(host_allowed("GitHub.COM", &p));
        assert!(host_allowed("github.com.", &p));
        assert!(host_allowed("Static.Crates.IO", &patterns(&["*.CRATES.io"])));
        // No suffix tricks: evilcrates.io is not *.crates.io.
        assert!(!host_allowed("evilcrates.io", &p));
        assert!(!host_allowed("crates.io.evil.example", &p));
        // No match, and the empty list allows nothing.
        assert!(!host_allowed("example.com", &p));
        assert!(!host_allowed("github.com", &[]));
    }

    #[test]
    fn head_and_target_parsing() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
        assert_eq!(find_head_end(b"partial\r\n"), None);
        assert_eq!(split_host_port("example.com:443", None), Some(("example.com".into(), 443)));
        assert_eq!(split_host_port("example.com", None), None); // CONNECT needs a port
        assert_eq!(split_host_port("example.com", Some(80)), Some(("example.com".into(), 80)));
        assert_eq!(split_host_port("[::1]:8080", None), Some(("::1".into(), 8080)));
        assert_eq!(
            http_target("http://example.com:8080/path", ""),
            Some(("example.com".into(), 8080))
        );
        assert_eq!(http_target("http://example.com/path", ""), Some(("example.com".into(), 80)));
        // Origin-form falls back to the Host header.
        assert_eq!(
            http_target("/path", "GET /path HTTP/1.1\r\nHost: fallback.example:81\r\n"),
            Some(("fallback.example".into(), 81))
        );
        assert_eq!(http_target("/path", "GET /path HTTP/1.1\r\n"), None);
    }

    /// Spawn a proxy loop on an ephemeral port with the given allowlist.
    async fn test_proxy(allow: &[&str]) -> u16 {
        let patterns: &'static [String] = Box::leak(Box::new(patterns(allow)));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(handle_conn(stream, patterns));
            }
        });
        port
    }

    async fn read_until_head_end(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        while find_head_end(&buf).is_none() {
            let mut chunk = [0u8; 1024];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn connect_tunnels_bytes_both_ways() {
        // Local TCP "origin": reads 4 bytes, answers `pong`.
        let origin = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = origin.accept().await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
            s.write_all(b"pong").await.unwrap();
        });

        let proxy = test_proxy(&["127.0.0.1"]).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        client
            .write_all(format!("CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let reply = read_until_head_end(&mut client).await;
        assert!(reply.starts_with("HTTP/1.1 200"), "expected tunnel established, got: {reply}");
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn connect_to_unlisted_host_is_403() {
        let proxy = test_proxy(&["127.0.0.1"]).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        client.write_all(b"CONNECT evil.example:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 403"), "got: {reply}");
        assert!(
            reply.contains("hotl egress: \"evil.example\" is not in [network].allow"),
            "the deny body must be the errors-as-prompts message: {reply}"
        );
    }

    #[tokio::test]
    async fn absolute_form_get_forwards_to_origin() {
        // Local HTTP origin: consume the head, answer 200 `ok`.
        let origin = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = origin.accept().await.unwrap();
            let head = read_until_head_end(&mut s).await;
            assert!(head.starts_with("GET "), "origin should see the GET: {head}");
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await.unwrap();
        });

        let proxy = test_proxy(&["127.0.0.1"]).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        client
            .write_all(
                format!("GET http://127.0.0.1:{origin_port}/x HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "got: {reply}");
        assert!(reply.ends_with("ok"));
    }

    #[tokio::test]
    async fn malformed_head_is_400() {
        let proxy = test_proxy(&["127.0.0.1"]).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        // A complete head with no CONNECT target, no absolute-form URI, and
        // no Host header: nothing to check a policy against.
        client.write_all(b"garbage\r\n\r\n").await.unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 400"), "got: {reply}");
    }
}
