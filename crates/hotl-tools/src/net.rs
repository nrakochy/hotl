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

/// A cooperating client sends its whole request head immediately. Anything
/// slower is malformed or hostile, and without this an unfinished head pins a
/// task and a socket for the life of the process (T3-31).
const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Ceiling on live proxied connections. Beyond it the proxy answers 503
/// rather than accepting unbounded work — the same Layer-B discipline
/// `SessionConcurrency` applies to subprocesses and requests.
const MAX_PROXY_CONNS: usize = 64;

/// Hosts an allowlist starts with: the package registries, their CDNs, and the
/// git forges a build reaches without anyone deciding to reach them. Bounds
/// accidents and drive-by fetches — NOT exfiltration: `github.com` is
/// bidirectional and a gist push leaves through it (docs/SECURITY.md).
///
/// Exact hosts, never wildcards: a default nobody can enumerate is a default
/// nobody can audit. Enforced by `starter_allow_has_no_wildcards`.
pub const STARTER_ALLOW: &[&str] = &[
    // Rust
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "static.rust-lang.org",
    "sh.rustup.rs",
    "docs.rs",
    // Node
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    // Python
    "pypi.org",
    "files.pythonhosted.org",
    // Go
    "proxy.golang.org",
    "sum.golang.org",
    // Ruby
    "rubygems.org",
    // Forges
    "github.com",
    "api.github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
    "gitlab.com",
];

/// The one normalization every host comparison uses: lowercase, trailing root
/// dot stripped. Shared because a table that normalizes differently from
/// `host_matches` produces a prompt that reappears forever (0026 watch-out 5).
///
/// Deliberately does not strip a port: callers already split host from port
/// (`split_host_port`, `http_target`), and stripping here would silently
/// accept `example.com:443` as a pattern.
pub fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// An RFC 7617 `Basic` credential value, ready to compare against a
/// `Proxy-Authorization` header. The server side compares against this
/// precomputed string, so no decoder is required.
fn basic_auth_value(user: &str, pass: &str) -> String {
    format!(
        "Basic {}",
        crate::b64::encode(format!("{user}:{pass}").as_bytes())
    )
}

/// The per-session proxy token: 128 bits from a SplitMix64 seeded on the
/// wall clock, the pid, and a stack address.
///
/// **Not cryptographic, and deliberately so.** The credential guards a
/// loopback listener against *other local processes* for the lifetime of one
/// session; it is visible to anything running as this user (it rides the
/// child's `HTTP_PROXY`), which is precisely the boundary it does not claim to
/// defend. Predicting it buys an attacker the egress allowlist they could
/// already reach by reading the environment.
fn proxy_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let local = 0u64;
        let mut state =
            nanos ^ ((std::process::id() as u64) << 32) ^ (&local as *const u64 as usize as u64);
        let mut out = String::with_capacity(32);
        for _ in 0..2 {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            out.push_str(&format!("{z:016x}"));
        }
        out
    })
}

/// The credential `handle_conn` requires, or `None` under
/// `HOTL_PROXY_AUTH=off` (for a client that honors the proxy host but drops
/// proxy credentials).
fn proxy_credential() -> Option<&'static str> {
    if std::env::var("HOTL_PROXY_AUTH").is_ok_and(|v| v == "off") {
        return None;
    }
    static CRED: OnceLock<String> = OnceLock::new();
    Some(CRED.get_or_init(|| basic_auth_value("hotl", proxy_token())))
}

/// The `user:pass` for the proxy URL handed to children, or `None` when auth
/// is off. `sandbox::apply_proxy_env` formats it into `HTTP_PROXY`.
pub(crate) fn proxy_user_info() -> Option<String> {
    proxy_credential().map(|_| format!("hotl:{}", proxy_token()))
}

/// Length-independent-of-content comparison over equal-length inputs.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Does the head carry `name: expected`? Compared in constant time.
fn header_matches(head: &str, name: &str, expected: &str) -> bool {
    head.lines().skip(1).any(|line| {
        let Some((k, v)) = line.split_once(':') else {
            return false;
        };
        k.trim().eq_ignore_ascii_case(name) && ct_eq(v.trim().as_bytes(), expected.as_bytes())
    })
}

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
        // A partial *fs* floor still has a full net story: the net ABI is
        // checked independently, and hard.
        #[cfg(target_os = "linux")]
        SandboxStatus::Enforced("landlock") | SandboxStatus::Enforced("landlock(partial)") => {
            crate::sandbox::landlock_net_supported()
        }
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
fn host_matches(host: &str, patterns: &[String]) -> bool {
    let host = normalize_host(host);
    patterns.iter().any(|pattern| {
        let pattern = pattern.to_ascii_lowercase();
        match pattern.strip_prefix("*.") {
            Some(apex) => host == apex || host.ends_with(&format!(".{apex}")),
            None => host == pattern,
        }
    })
}

/// The verdict for a pre-request egress check (`web_fetch`/`web_search`): can
/// this host be reached *before* a socket is opened, with no subprocess and
/// no proxy round-trip — just the same policy bash's proxy consults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostVerdict {
    /// The policy explicitly allows this host (allowlist match).
    Allowed,
    /// The policy explicitly refuses this host; the reason is prompt-shaped
    /// (tells the model what to do: add the host, or change the policy).
    Denied(String),
    /// No restriction is configured (`Open`) — the caller falls back to the
    /// ordinary permission ask; this is not itself a grant.
    NoPolicy,
}

/// Pure decision function, testable without touching the process-wide
/// `OnceLock` (which is set-once and installed for real only at startup).
fn verdict_for(host: &str, policy: &EgressPolicy) -> HostVerdict {
    match policy {
        EgressPolicy::Open => HostVerdict::NoPolicy,
        EgressPolicy::Off => HostVerdict::Denied(format!(
            "egress is off ([network] egress = \"off\"); \"{host}\" is unreachable — \
             set egress = \"allowlist\" and add it, or egress = \"open\""
        )),
        EgressPolicy::Allowlist(patterns) => {
            if host_matches(host, patterns) {
                HostVerdict::Allowed
            } else {
                HostVerdict::Denied(format!(
                    "\"{host}\" is not in [network] allow; add it or use egress = \"open\""
                ))
            }
        }
    }
}

/// Pre-request egress check shared by `web_fetch`/`web_search` (and anything
/// else that wants to know "can I reach this host" before spending a socket
/// or a subprocess): reads the same process-wide policy bash's proxy
/// consults, so there is exactly one egress authority, never a second
/// tool-local allowlist.
pub fn host_allowed(host: &str) -> HostVerdict {
    verdict_for(host, policy())
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
    let credential = proxy_credential();
    let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_PROXY_CONNS));
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                // Accepts stay cheap; *live* connections are capped. Over the
                // limit the client gets an immediate 503 rather than joining
                // an unbounded task queue.
                Ok((stream, _)) => match limit.clone().try_acquire_owned() {
                    Ok(permit) => {
                        tokio::spawn(handle_conn(stream, patterns, permit, credential));
                    }
                    Err(_) => {
                        tokio::spawn(async move {
                            let mut s = stream;
                            respond(
                                &mut s,
                                "503 Service Unavailable",
                                "hotl egress proxy: too many concurrent connections",
                            )
                            .await;
                        });
                    }
                },
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
async fn handle_conn(
    mut client: TcpStream,
    patterns: &'static [String],
    _permit: tokio::sync::OwnedSemaphorePermit, // released when the conn ends
    credential: Option<&'static str>,
) {
    // Read until the end of the head (CRLFCRLF), capped and time-bounded.
    let read_head = async {
        let mut buf: Vec<u8> = Vec::with_capacity(1024);
        loop {
            if let Some(end) = find_head_end(&buf) {
                return Some((buf, end));
            }
            if buf.len() >= MAX_HEAD {
                return None;
            }
            let mut chunk = [0u8; 4096];
            match client.read(&mut chunk).await {
                Ok(0) | Err(_) => return None, // client went away mid-head
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    };
    let Ok(head_read) = tokio::time::timeout(HEAD_TIMEOUT, read_head).await else {
        return respond(
            &mut client,
            "408 Request Timeout",
            "request head not completed",
        )
        .await;
    };
    let Some((buf, head_end)) = head_read else {
        return;
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    // INVARIANT: only a caller holding this session's credential can spend the
    // egress allowlist. Enforced by `the_proxy_requires_its_credential`.
    if let Some(expected) = credential {
        if !header_matches(&head, "proxy-authorization", expected) {
            return respond(
                &mut client,
                "407 Proxy Authentication Required",
                "hotl egress proxy: this proxy serves only the hotl session that started it",
            )
            .await;
        }
    }
    let mut parts = head.lines().next().unwrap_or("").split_ascii_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    if method == "CONNECT" {
        // CONNECT host:port — establish a blind tunnel (TLS goes through here).
        let Some((host, port)) = split_host_port(target, None) else {
            return respond(&mut client, "400 Bad Request", "malformed CONNECT target").await;
        };
        if !host_matches(&host, patterns) {
            return deny_host(&mut client, &host).await;
        }
        let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)).await else {
            return respond(&mut client, "502 Bad Gateway", "upstream connect failed").await;
        };
        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
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
    if !host_matches(&host, patterns) {
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

/// The `Host` header, or `None` when there is not exactly one. Two `Host`
/// headers let the policy check one value while the upstream honors the other
/// — request smuggling by construction, so it is refused (400), not resolved.
///
/// INVARIANT: exactly one `Host` header reaches the policy check. Enforced by
/// `duplicate_host_headers_are_refused_not_silently_first_wins`.
fn host_header(head: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("host") {
            continue;
        }
        if found.is_some() {
            return None; // more than one
        }
        found = Some(value.trim().to_string());
    }
    found
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
        assert!(host_matches("github.com", &p));
        assert!(!host_matches("api.github.com", &p)); // exact is not a wildcard
                                                      // Wildcard covers the apex and every subdomain depth.
        assert!(host_matches("crates.io", &p));
        assert!(host_matches("static.crates.io", &p));
        assert!(host_matches("a.b.crates.io", &p));
        // Case-insensitive both sides; trailing dot stripped.
        assert!(host_matches("GitHub.COM", &p));
        assert!(host_matches("github.com.", &p));
        assert!(host_matches(
            "Static.Crates.IO",
            &patterns(&["*.CRATES.io"])
        ));
        // No suffix tricks: evilcrates.io is not *.crates.io.
        assert!(!host_matches("evilcrates.io", &p));
        assert!(!host_matches("crates.io.evil.example", &p));
        // No match, and the empty list allows nothing.
        assert!(!host_matches("example.com", &p));
        assert!(!host_matches("github.com", &[]));
    }

    /// Task 1: `host_allowed` is the pre-request egress check `web_fetch`/
    /// `web_search` consult — same policy bash's proxy uses, no subprocess,
    /// no second allowlist. `Open` (the default) yields `NoPolicy` — the
    /// caller still asks; `Off` denies every host; `Allowlist` matches like
    /// the proxy does, with a prompt-shaped reason on denial.
    #[test]
    fn host_allowed_reads_the_configured_policy() {
        let allow = EgressPolicy::Allowlist(patterns(&["github.com", "*.crates.io"]));
        assert_eq!(verdict_for("github.com", &allow), HostVerdict::Allowed);
        assert_eq!(verdict_for("api.crates.io", &allow), HostVerdict::Allowed);
        match verdict_for("evil.com", &allow) {
            HostVerdict::Denied(reason) => {
                assert!(reason.contains("evil.com") && reason.contains("[network] allow"));
            }
            other => panic!("expected Denied, got {other:?}"),
        }

        assert_eq!(
            verdict_for("anything.example", &EgressPolicy::Open),
            HostVerdict::NoPolicy
        );

        match verdict_for("anything.example", &EgressPolicy::Off) {
            HostVerdict::Denied(reason) => assert!(reason.contains("egress is off")),
            other => panic!("expected Denied, got {other:?}"),
        }

        // The public entrypoint reads the (unset-in-tests, so default Open)
        // process-wide policy.
        assert_eq!(host_allowed("anything.example"), HostVerdict::NoPolicy);
    }

    /// 0026 watch-out 8: every host on the starter list is one hotl asserts is
    /// fine to reach by default. A wildcard would make that set unenumerable,
    /// so widening it has to be a deliberate edit to this test too.
    #[test]
    fn starter_allow_has_no_wildcards() {
        assert!(STARTER_ALLOW.iter().all(|host| !host.contains('*')));
    }

    #[test]
    fn starter_allow_is_lowercase_and_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for host in STARTER_ALLOW {
            assert_eq!(*host, normalize_host(host), "{host} is not normalized");
            assert!(seen.insert(*host), "{host} appears twice");
        }
    }

    #[test]
    fn head_and_target_parsing() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
        assert_eq!(find_head_end(b"partial\r\n"), None);
        assert_eq!(
            split_host_port("example.com:443", None),
            Some(("example.com".into(), 443))
        );
        assert_eq!(split_host_port("example.com", None), None); // CONNECT needs a port
        assert_eq!(
            split_host_port("example.com", Some(80)),
            Some(("example.com".into(), 80))
        );
        assert_eq!(
            split_host_port("[::1]:8080", None),
            Some(("::1".into(), 8080))
        );
        assert_eq!(
            http_target("http://example.com:8080/path", ""),
            Some(("example.com".into(), 8080))
        );
        assert_eq!(
            http_target("http://example.com/path", ""),
            Some(("example.com".into(), 80))
        );
        // Origin-form falls back to the Host header.
        assert_eq!(
            http_target(
                "/path",
                "GET /path HTTP/1.1\r\nHost: fallback.example:81\r\n"
            ),
            Some(("fallback.example".into(), 81))
        );
        assert_eq!(http_target("/path", "GET /path HTTP/1.1\r\n"), None);
    }

    /// Spawn a proxy loop on an ephemeral port with the given allowlist and
    /// (optionally) a required credential — the same shape `start_proxy`
    /// builds, so the tests exercise the production `handle_conn`.
    async fn test_proxy_with(allow: &[&str], credential: Option<&'static str>) -> u16 {
        let patterns: &'static [String] = Box::leak(Box::new(patterns(allow)));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_PROXY_CONNS));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if let Ok(permit) = limit.clone().try_acquire_owned() {
                    tokio::spawn(handle_conn(stream, patterns, permit, credential));
                }
            }
        });
        port
    }

    async fn test_proxy(allow: &[&str]) -> u16 {
        test_proxy_with(allow, None).await
    }

    async fn test_proxy_authed(allow: &[&str], secret: &str) -> u16 {
        let expected: &'static str = Box::leak(basic_auth_value("hotl", secret).into_boxed_str());
        test_proxy_with(allow, Some(expected)).await
    }

    #[tokio::test]
    async fn an_unfinished_head_does_not_hold_the_connection_forever() {
        let proxy = test_proxy(&["127.0.0.1"]).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        client
            .write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\n")
            .await
            .unwrap(); // no CRLFCRLF
        let mut reply = String::new();
        let read = tokio::time::timeout(
            HEAD_TIMEOUT + std::time::Duration::from_secs(2),
            client.read_to_string(&mut reply),
        )
        .await;
        assert!(
            read.is_ok(),
            "the proxy must close an idle half-open request head"
        );
        assert!(
            reply.is_empty() || reply.starts_with("HTTP/1.1 408"),
            "got: {reply}"
        );
    }

    #[test]
    fn duplicate_host_headers_are_refused_not_silently_first_wins() {
        assert_eq!(
            host_header("GET / HTTP/1.1\r\nHost: a.example\r\n"),
            Some("a.example".to_string())
        );
        assert_eq!(
            host_header("GET / HTTP/1.1\r\nHost: a.example\r\nhost: b.example\r\n"),
            None,
            "two Host headers is a smuggling attempt, not a first-wins choice"
        );
        assert_eq!(
            http_target("/p", "GET /p HTTP/1.1\r\nHost: a\r\nHost: b\r\n"),
            None
        );
    }

    #[tokio::test]
    async fn the_proxy_requires_its_credential() {
        let proxy = test_proxy_authed(&["127.0.0.1"], "s3cret").await;
        // No credential: 407, and no tunnel.
        let mut anon = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        anon.write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut reply = String::new();
        anon.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 407"), "got: {reply}");
        // With it: the request reaches the policy check (403 for an unlisted host).
        let mut authed = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        let cred = basic_auth_value("hotl", "s3cret");
        authed
            .write_all(
                format!("CONNECT evil.example:443 HTTP/1.1\r\nproxy-authorization: {cred}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut reply = String::new();
        authed.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 403"), "got: {reply}");
    }

    #[test]
    fn basic_auth_encodes_like_rfc7617() {
        assert_eq!(basic_auth_value("hotl", "s3cret"), "Basic aG90bDpzM2NyZXQ=");
        assert_eq!(basic_auth_value("a", "b"), "Basic YTpi"); // 3 bytes → no pad
        assert_eq!(basic_auth_value("a", "bc"), "Basic YTpiYw=="); // 4 bytes → two pads
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
        assert!(
            reply.starts_with("HTTP/1.1 200"),
            "expected tunnel established, got: {reply}"
        );
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn connect_to_unlisted_host_is_403() {
        let proxy = test_proxy(&["127.0.0.1"]).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy)).await.unwrap();
        client
            .write_all(b"CONNECT evil.example:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
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
            assert!(
                head.starts_with("GET "),
                "origin should see the GET: {head}"
            );
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .await
                .unwrap();
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
