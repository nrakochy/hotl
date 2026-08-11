//! Server trust store (SECURITY.md §M3a first-use screen): approval is
//! recorded per server as a composite [`Fingerprint`] over
//! `(command, args, binary hash)`; changing any of the three re-raises the
//! protected ask.
//!
//! Hashing the *interpreter* alone was T2-7a in the 2026-07-25 evaluation: for
//! the dominant shape `command = "npx", args = ["-y", "@pkg/server"]` the
//! recorded trust was the SHA-256 of `npx`, so repointing the same server name
//! at a different package reused the grant with no re-prompt. Identical for
//! `node server.js`, `python -m …`, `uvx`, `docker run`.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::ServerConfig;

/// Everything that decides *what program runs*, captured in one read.
///
/// `Display` is what the human is shown; [`Fingerprint::key`] is what gets
/// persisted. Both derive from the same value, which is how H-07's
/// "shown == recorded, from one read" discipline is preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    command: String,
    args: Vec<String>,
    hash: String,
    /// Content hashes of any argument that resolves to an existing file — the
    /// script/module an interpreter actually runs (`node server.js`,
    /// `python x.py`). Hashing only `command` (the interpreter, which never
    /// changes) left the real code outside the fingerprint (Vuln 5). Empty for
    /// args that name no local file (`-m pkg`, `npx @pkg`, a `docker` image).
    arg_hashes: Vec<(String, String)>,
}

impl Fingerprint {
    /// Reads the binary **once**. Callers must not re-derive the parts.
    pub fn of(cfg: &ServerConfig) -> Self {
        let arg_hashes = cfg
            .args
            .iter()
            .filter_map(|a| {
                let p = resolve(a);
                p.is_file().then(|| (a.clone(), file_hash(&p)))
            })
            .collect();
        Self {
            command: cfg.command.clone(),
            args: cfg.args.clone(),
            hash: binary_hash(&cfg.command),
            arg_hashes,
        }
    }

    /// True when any argument resolves to a file inside `root` — the
    /// agent-writable workspace. Such a server's script can be rewritten by an
    /// auto-allowed in-workspace edit, so its trust must not be persisted
    /// durably (`TrustStore::record` refuses it); it is re-screened each session.
    pub fn has_arg_under(&self, root: &Path) -> bool {
        self.arg_hashes
            .iter()
            .any(|(arg, _)| dunce::canonicalize(resolve(arg)).is_ok_and(|p| p.starts_with(root)))
    }

    /// False when the binary could not be read. A grant is never recorded for
    /// an unhashable binary, and one is never honoured (T2-7b).
    pub fn is_hashed(&self) -> bool {
        self.hash.starts_with("sha256:")
    }

    /// The recorded value. Versioned (`fp2:`) so a future scheme change
    /// invalidates old grants by construction rather than by accident. NUL
    /// cannot appear in an argv element, so the encoding is injective.
    pub fn key(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.command.as_bytes());
        hasher.update([0]);
        for arg in &self.args {
            hasher.update(arg.as_bytes());
            hasher.update([0]);
        }
        hasher.update(self.hash.as_bytes());
        hasher.update([0]);
        // fp2: the scheme now folds in the content hash of any file-resolving
        // arg. Old `fp1:` grants no longer match — the safe direction (they
        // re-raise the screen once).
        for (arg, h) in &self.arg_hashes {
            hasher.update(arg.as_bytes());
            hasher.update([0]);
            hasher.update(h.as_bytes());
            hasher.update([0]);
        }
        format!("fp2:{:x}", hasher.finalize())
    }
}

impl fmt::Display for Fingerprint {
    /// The screen text. `tool.rs`'s `why` string reuses this verbatim rather
    /// than re-deriving the parts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "binary: {}", self.command)?;
        if !self.args.is_empty() {
            write!(f, "\nargs: {:?}", self.args)?;
        }
        write!(f, "\n  {}", self.hash)?;
        // Show the script/module each interpreter arg actually runs, so the
        // human screens the real code, not just the interpreter (Vuln 5).
        for (arg, h) in &self.arg_hashes {
            write!(f, "\n  {arg}: {h}")?;
        }
        Ok(())
    }
}

/// What hotl will do the next time a turn reaches this server. Reporting-only:
/// every variant is derived from [`TrustStore::is_trusted`], so a roster can
/// never claim something the gate would contradict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    /// A grant is on file and the program still matches it. No screen.
    Trusted,
    /// No grant, or the program changed since the grant. Screens on first use.
    Untrusted,
    /// The binary could not be read, so no integrity check applies (T2-7b).
    /// Never recorded, never honoured — screens every time.
    Unhashable,
    /// A file-resolving arg lives in the agent-writable workspace, so trust is
    /// never persisted durably (Vuln 5). Screens once per session, by design.
    SessionOnly,
}

impl TrustState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "screens on first use",
            Self::Unhashable => "unreadable binary — will ask every time",
            Self::SessionOnly => "session-only (workspace script — never persisted)",
        }
    }
}

pub struct TrustStore {
    path: PathBuf,
    /// server name → approved fingerprint key
    approved: HashMap<String, String>,
}

impl TrustStore {
    pub fn load(config_dir: &Path) -> Self {
        Self::load_reporting(config_dir).0
    }

    /// `load` plus the warnings it would otherwise swallow. A trust.toml that
    /// fails to parse discards **every** grant, which the user should hear
    /// about rather than discover as a wave of re-prompts.
    pub fn load_reporting(config_dir: &Path) -> (Self, Vec<String>) {
        let path = config_dir.join("trust.toml");
        let mut warnings = Vec::new();
        let approved = match std::fs::read_to_string(&path) {
            Ok(raw) => match toml::from_str::<HashMap<String, String>>(&raw) {
                Ok(map) => map,
                Err(e) => {
                    warnings.push(format!(
                        "trust.toml at {} is unreadable ({e}); every MCP server will \
                         re-ask for approval. Move it aside to clear the warning.",
                        path.display()
                    ));
                    HashMap::new()
                }
            },
            // Absent is the normal first-run case, not a warning.
            Err(_) => HashMap::new(),
        };
        (Self { path, approved }, warnings)
    }

    /// INVARIANT: the trust key covers (command, args, binary hash) —
    /// repointing a server name at a different package re-raises the protected
    /// ask. Enforced by `args_are_part_of_the_trust_key`.
    ///
    /// Fails closed on an unhashable binary even if some stale key matched: no
    /// integrity check applies, so there is nothing to trust.
    pub fn is_trusted(&self, server: &str, fp: &Fingerprint) -> bool {
        fp.is_hashed() && self.approved.get(server) == Some(&fp.key())
    }

    /// Record approval durably.
    ///
    /// INVARIANT: a grant is never recorded for a binary that could not be
    /// hashed. Enforced by `an_unavailable_hash_is_never_recorded`.
    /// INVARIANT: trust.toml is replaced by rename, never truncated in place,
    /// and is 0600. Enforced by
    /// `the_trust_file_is_written_atomically_and_privately`.
    pub fn record(&mut self, server: &str, fp: &Fingerprint) -> Result<(), String> {
        if !fp.is_hashed() {
            return Err(format!(
                "the binary for `{server}` could not be hashed ({}); hotl will ask \
                 again every time. Check that `{}` exists and is readable.",
                fp.hash, fp.command
            ));
        }
        self.approved.insert(server.to_string(), fp.key());
        let raw = toml::to_string(&self.approved).map_err(|e| format!("{e}"))?;
        self.persist(&raw)
    }

    /// Drop a grant. Revocation only ever *reduces* privilege, which is why it
    /// is the one mutation `hotl mcp` is allowed to make. `Ok(false)` when the
    /// server held no grant — not an error, the end state is the same.
    ///
    /// INVARIANT: forgetting one server leaves every other grant intact.
    /// Enforced by `forget_revokes_only_the_named_server`.
    pub fn forget(&mut self, server: &str) -> Result<bool, String> {
        if self.approved.remove(server).is_none() {
            return Ok(false);
        }
        let raw = toml::to_string(&self.approved).map_err(|e| format!("{e}"))?;
        self.persist(&raw)?;
        Ok(true)
    }

    /// Recorded grants, for spotting ones whose server left the config.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.approved.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// What the next turn will do. `Trusted` is decided by [`Self::is_trusted`]
    /// and checked first, so this never disagrees with the gate — a durable
    /// grant that predates a config change moving the script into `workspace`
    /// reports honestly as trusted, because that is what hotl will honour.
    pub fn state(&self, server: &str, fp: &Fingerprint, workspace: &Path) -> TrustState {
        if !fp.is_hashed() {
            return TrustState::Unhashable;
        }
        if self.is_trusted(server, fp) {
            return TrustState::Trusted;
        }
        if fp.has_arg_under(workspace) {
            return TrustState::SessionOnly;
        }
        TrustState::Untrusted
    }

    /// Temp file in the same directory, then rename: a crash mid-write leaves
    /// either the old file or the new one, never a truncated one that `load`
    /// silently discards (T3-18).
    fn persist(&self, raw: &str) -> Result<(), String> {
        use std::io::Write;

        let dir = self.path.parent().ok_or("trust.toml has no parent")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("could not create {dir:?}: {e}"))?;
        let tmp = dir.join(format!(
            "trust.toml.{}.{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));

        let write = || -> std::io::Result<()> {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp)?;
            file.write_all(raw.as_bytes())?;
            file.sync_all()
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("could not write {tmp:?}: {e}"));
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("could not replace {:?}: {e}", self.path));
        }
        Ok(())
    }
}

/// SHA-256 of the server binary, resolving bare names through PATH the same
/// way the shell will. Unreadable binaries hash as `unavailable:` — the screen
/// shows that honestly, and [`TrustStore::record`] refuses to persist it.
/// This is the primitive; [`Fingerprint`] is the trust key.
pub fn binary_hash(command: &str) -> String {
    file_hash(&resolve(command))
}

/// SHA-256 of a file's contents, or `unavailable:<err>` if it can't be read.
fn file_hash(path: &Path) -> String {
    let read = |path: &Path| -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(format!("sha256:{:x}", hasher.finalize()))
    };
    read(path).unwrap_or_else(|e| format!("unavailable:{e}"))
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
        let bin = dir.path().join("server");
        std::fs::write(&bin, b"original").unwrap();
        let path = bin.to_str().unwrap();

        let before = Fingerprint::of(&cfg(path, &[]));
        let mut store = TrustStore::load(dir.path());
        assert!(!store.is_trusted("docs", &before));
        store.record("docs", &before).unwrap();
        assert!(store.is_trusted("docs", &before));
        // Reload from disk: durable.
        let store2 = TrustStore::load(dir.path());
        assert!(store2.is_trusted("docs", &before));
        // The binary changed on disk: NOT trusted (content-hash revocation).
        std::fs::write(&bin, b"swapped!").unwrap();
        let after = Fingerprint::of(&cfg(path, &[]));
        assert_ne!(before.key(), after.key());
        assert!(!store2.is_trusted("docs", &after));
    }

    fn cfg(command: &str, args: &[&str]) -> crate::config::ServerConfig {
        crate::config::ServerConfig {
            name: "docs".into(),
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            description: String::new(),
        }
    }

    #[test]
    fn editing_the_server_script_revokes_trust() {
        // Vuln 5: hashing only the interpreter left the *script* it runs outside
        // the fingerprint, so a prompt-injected edit to server.js reused the
        // grant with no re-screen. A file-resolving arg's content is hashed too.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("server.js");
        std::fs::write(&script, b"console.log('v1')").unwrap();
        let cfg = cfg("/bin/sh", &[script.to_str().unwrap()]);
        let before = Fingerprint::of(&cfg).key();
        std::fs::write(&script, b"console.log('MALICIOUS')").unwrap();
        let after = Fingerprint::of(&cfg).key();
        assert_ne!(
            before, after,
            "editing the server script must re-raise the screen"
        );
    }

    #[test]
    fn a_workspace_script_is_flagged_as_agent_writable() {
        // Vuln 5: a server whose script resolves inside a given root is flagged
        // so `tool.rs` can refuse to persist its trust durably.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("s.js");
        std::fs::write(&script, b"x").unwrap();
        let fp = Fingerprint::of(&cfg("/bin/sh", &[script.to_str().unwrap()]));
        let root = dunce::canonicalize(dir.path()).unwrap();
        assert!(fp.has_arg_under(&root), "script under root must be flagged");
        assert!(
            !fp.has_arg_under(std::path::Path::new("/no/such/root")),
            "an unrelated root must not match"
        );
    }

    #[test]
    fn args_are_part_of_the_trust_key() {
        // T2-7a: the dominant MCP shape is `npx -y <package>`; the package is
        // the thing being trusted, and it lives in args.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("npx");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let npx = bin.to_str().unwrap();
        let good = Fingerprint::of(&cfg(npx, &["-y", "@good/mcp-server"]));
        let evil = Fingerprint::of(&cfg(npx, &["-y", "@evil/mcp-server"]));
        assert_ne!(
            good.key(),
            evil.key(),
            "same interpreter, different package"
        );
        let mut store = TrustStore::load(dir.path());
        store.record("docs", &good).unwrap();
        assert!(store.is_trusted("docs", &good));
        assert!(
            !store.is_trusted("docs", &evil),
            "repointing args must re-prompt"
        );
    }

    #[test]
    fn an_unavailable_hash_is_never_recorded() {
        // T2-7b: `unavailable:{e}` used to be persisted and then matched forever.
        let dir = tempfile::tempdir().unwrap();
        let fp = Fingerprint::of(&cfg("/definitely/not/here", &[]));
        assert!(!fp.is_hashed());
        let mut store = TrustStore::load(dir.path());
        let err = store.record("docs", &fp).expect_err("must refuse");
        assert!(
            err.contains("could not be hashed"),
            "errors-as-prompt: {err}"
        );
        assert!(
            !store.is_trusted("docs", &fp),
            "fail closed: ask every time"
        );
        let reloaded = TrustStore::load(dir.path());
        assert!(!reloaded.is_trusted("docs", &fp), "nothing persisted");
    }

    #[cfg(unix)]
    #[test]
    fn the_trust_file_is_written_atomically_and_privately() {
        // T3-18: a crash mid-write must never leave truncated TOML that `load`
        // swallows, discarding every grant.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("server");
        std::fs::write(&bin, b"x").unwrap();
        let fp = Fingerprint::of(&cfg(bin.to_str().unwrap(), &[]));
        let mut store = TrustStore::load(dir.path());
        store.record("a", &fp).unwrap();
        store.record("b", &fp).unwrap();
        let path = dir.path().join("trust.toml");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "trust.toml is a security record");
        // No temp files left behind.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("trust.toml."))
            .collect();
        assert!(strays.is_empty(), "temp files must be renamed, not leaked");
        assert!(TrustStore::load(dir.path()).is_trusted("b", &fp));
    }

    #[test]
    fn a_corrupt_trust_file_is_loud_not_silent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trust.toml"), b"this is not = valid toml [").unwrap();
        let (store, warnings) = TrustStore::load_reporting(dir.path());
        assert!(
            !warnings.is_empty(),
            "discarding every grant must be reported"
        );
        assert!(warnings[0].contains("trust.toml"));
        let _ = store;
    }

    #[test]
    fn forget_revokes_only_the_named_server() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("server");
        std::fs::write(&bin, b"x").unwrap();
        let fp = Fingerprint::of(&cfg(bin.to_str().unwrap(), &[]));
        let mut store = TrustStore::load(dir.path());
        store.record("a", &fp).unwrap();
        store.record("b", &fp).unwrap();

        assert!(store.forget("a").unwrap(), "a held a grant");
        assert!(!store.is_trusted("a", &fp));
        assert!(store.is_trusted("b", &fp), "b is untouched");
        // Durable, and forgetting an unknown name is a no-op, not an error.
        let reloaded = TrustStore::load(dir.path());
        assert!(!reloaded.is_trusted("a", &fp) && reloaded.is_trusted("b", &fp));
        assert!(!TrustStore::load(dir.path()).forget("ghost").unwrap());
    }

    #[test]
    fn state_reports_what_the_gate_will_do() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = Path::new("/no/such/root");
        let bin = dir.path().join("server");
        std::fs::write(&bin, b"x").unwrap();
        let fp = Fingerprint::of(&cfg(bin.to_str().unwrap(), &[]));
        let mut store = TrustStore::load(dir.path());

        assert_eq!(store.state("docs", &fp, elsewhere), TrustState::Untrusted);
        store.record("docs", &fp).unwrap();
        assert_eq!(store.state("docs", &fp, elsewhere), TrustState::Trusted);

        // An unreadable binary is never honoured, even against a stale key.
        let gone = Fingerprint::of(&cfg("/definitely/not/here", &[]));
        assert_eq!(
            store.state("docs", &gone, elsewhere),
            TrustState::Unhashable
        );

        // A script under the workspace is never durably trusted (Vuln 5).
        // Use a hashable command (`bin`, written above): an unreadable one like
        // `/bin/sh` short-circuits to Unhashable before the workspace check,
        // and `/bin/sh` does not exist on Windows anyway.
        let script = dir.path().join("s.js");
        std::fs::write(&script, b"x").unwrap();
        let ws = Fingerprint::of(&cfg(bin.to_str().unwrap(), &[script.to_str().unwrap()]));
        let root = dunce::canonicalize(dir.path()).unwrap();
        assert_eq!(store.state("local", &ws, &root), TrustState::SessionOnly);
    }

    #[test]
    fn entries_lists_recorded_grants() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("server");
        std::fs::write(&bin, b"x").unwrap();
        let fp = Fingerprint::of(&cfg(bin.to_str().unwrap(), &[]));
        let mut store = TrustStore::load(dir.path());
        store.record("docs", &fp).unwrap();
        let listed: Vec<_> = store.entries().map(|(n, _)| n.to_string()).collect();
        assert_eq!(listed, vec!["docs".to_string()]);
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
