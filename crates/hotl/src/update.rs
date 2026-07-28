//! `hotl update` — fetch the latest release and replace this binary.
//!
//! Only installs with no bookkeeping (dist installer, hand-unpacked tarball)
//! are replaced. Channels with an owning package manager are detected and
//! delegated — overwriting behind cargo's or nix's back desyncs them.

use std::path::Path;

/// No env override on purpose: a settable update origin is supply-chain
/// surface. Tests pass their own base URL to the inner functions.
const RELEASES: &str = "https://github.com/nrakochy/hotl/releases";

/// How this binary got onto disk. Decides whether `update` may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Nix,
    SourceBuild,
    Homebrew,
    Cargo,
    /// Dist installer or hand-unpacked tarball. Self-replaceable.
    Prebuilt,
}

/// `crates_toml` is the *contents* of `$CARGO_HOME/.crates.toml`, passed in to
/// keep this a pure function.
fn classify(exe: &Path, cargo_home: Option<&Path>, crates_toml: Option<&str>) -> Channel {
    if exe.components().any(|c| c.as_os_str() == "nix") && exe.starts_with("/nix/store") {
        return Channel::Nix;
    }
    // Match the `target/<profile>` pair, not a bare `target` segment, so a
    // binary merely living under a dir named `target` doesn't qualify.
    if let (Some(profile), Some(target)) = (
        exe.parent().and_then(Path::file_name),
        exe.parent()
            .and_then(Path::parent)
            .and_then(Path::file_name),
    ) {
        if target == "target" && (profile == "debug" || profile == "release") {
            return Channel::SourceBuild;
        }
    }
    if exe.components().any(|c| c.as_os_str() == "Cellar") {
        return Channel::Homebrew;
    }
    // Only cargo's own bin dir may be claimed by a `.crates.toml` record.
    let in_cargo_bin = cargo_home.is_some_and(|h| exe.parent() == Some(&h.join("bin")));
    if in_cargo_bin && crates_toml.is_some_and(cargo_tracks_hotl) {
        return Channel::Cargo;
    }
    Channel::Prebuilt
}

/// `[v1]` keys look like `"hotl 0.7.1 (registry+https://…)" = ["hotl"]`. Match
/// the package name off the key so `hotl-something` doesn't count.
fn cargo_tracks_hotl(contents: &str) -> bool {
    let Ok(doc) = contents.parse::<toml::Value>() else {
        return false;
    };
    let Some(v1) = doc.get("v1").and_then(toml::Value::as_table) else {
        return false;
    };
    v1.keys()
        .any(|k| k.split_whitespace().next() == Some("hotl"))
}

/// The subset of `dist-manifest.json` the updater reads.
#[derive(Debug, serde::Deserialize)]
struct Manifest {
    announcement_tag: Option<String>,
    #[serde(default)]
    announcement_is_prerelease: bool,
    #[serde(default)]
    artifacts: std::collections::BTreeMap<String, ManifestArtifact>,
}

#[derive(Debug, serde::Deserialize)]
struct ManifestArtifact {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    target_triples: Vec<String>,
    #[serde(default)]
    assets: Vec<ManifestAsset>,
    #[serde(default)]
    checksums: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, serde::Deserialize)]
struct ManifestAsset {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// `pub` for the integration test; `mod update` is private, so nothing leaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Release asset filename, e.g. `hotl-aarch64-apple-darwin.tar.gz`.
    pub name: String,
    pub sha256: String,
    /// Path of the executable *inside* the archive.
    pub exe_path: String,
}

/// `cfg!` rather than a build script — the workspace has no `build.rs`. musl
/// and Windows fall through: dist `targets` is glibc-only.
fn target_triple() -> Option<&'static str> {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Some("x86_64-unknown-linux-gnu")
    } else {
        None
    }
}

impl Manifest {
    fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("release manifest is not readable: {e}"))
    }

    /// The version this manifest announces, without the `v`.
    fn version(&self) -> Result<&str, String> {
        self.announcement_tag
            .as_deref()
            .map(|t| t.trim_start_matches('v'))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| "release manifest announces no version tag".to_string())
    }

    fn artifact_for(&self, triple: &str) -> Result<Artifact, String> {
        // Filter on `kind` too: the installer script carries every triple, so
        // matching the triple alone can select a shell script.
        let (name, art) = self
            .artifacts
            .iter()
            .find(|(_, a)| {
                a.kind == "executable-zip" && a.target_triples.iter().any(|t| t == triple)
            })
            .ok_or_else(|| format!("release {} has no prebuilt binary for {triple}", self.tag()))?;

        if !name.ends_with(".tar.gz") {
            return Err(format!(
                "release {} predates the .tar.gz artifact format ({name}); \
                 install it with the installer script instead:\n  \
                 curl --proto '=https' --tlsv1.2 -LsSf {RELEASES}/latest/download/hotl-installer.sh | sh",
                self.tag()
            ));
        }

        let sha256 = art
            .checksums
            .get("sha256")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| format!("{name} has no sha256 in the release manifest"))?
            .to_ascii_lowercase();

        let exe_path = art
            .assets
            .iter()
            .find(|a| a.kind.as_deref() == Some("executable"))
            .and_then(|a| a.path.clone())
            .ok_or_else(|| format!("{name} lists no executable in the release manifest"))?;

        Ok(Artifact {
            name: name.clone(),
            sha256,
            exe_path,
        })
    }

    fn tag(&self) -> &str {
        self.announcement_tag.as_deref().unwrap_or("(untagged)")
    }

    /// The version a plain `hotl update` should offer, if any. Prereleases are
    /// excluded here rather than relied on being absent from `/latest` —
    /// that's GitHub's policy, not our invariant. `--version` is the way in.
    fn upgrade_from(&self, current: &str) -> Result<Option<String>, String> {
        let latest = self.version()?;
        if self.announcement_is_prerelease || !is_newer(current, latest) {
            return Ok(None);
        }
        Ok(Some(latest.to_string()))
    }
}

/// Checked in-process before any decoding, so gzip/tar only see matched bytes.
///
/// Not provenance: the hash rides the same document, host, and TLS session as
/// the archive. Catches a corrupt download, not a replaced release. An offline
/// signature would cover that — see `docs/updating`.
fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    use sha2::Digest;
    let actual = format!("{:x}", sha2::Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(format!(
        "download failed its checksum — expected {expected}, got {actual}. \
         Nothing was installed."
    ))
}

/// Pull one file out of a gzipped tar, by its path inside the archive.
fn extract_executable(archive: &[u8], exe_path: &str) -> Result<Vec<u8>, String> {
    if !is_safe_entry_path(exe_path) {
        return Err(format!(
            "release manifest names an unsafe path inside the archive: {exe_path}"
        ));
    }
    let dec = flate2::read::GzDecoder::new(std::io::Cursor::new(archive));
    let mut tar = tar::Archive::new(dec);
    let entries = tar
        .entries()
        .map_err(|e| format!("release archive is not readable: {e}"))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| format!("release archive is not readable: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("release archive has an unreadable entry name: {e}"))?
            .to_string_lossy()
            .into_owned();
        if !is_safe_entry_path(&path) {
            return Err(format!("release archive has an unsafe path: {path}"));
        }
        if path != exe_path {
            continue;
        }
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut out)
            .map_err(|e| format!("could not read {exe_path} from the release archive: {e}"))?;
        return Ok(out);
    }
    Err(format!("release archive does not contain {exe_path}"))
}

/// Reject absolute paths and any `..` traversal in a tar entry name.
fn is_safe_entry_path(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && !path.starts_with('/')
        && !Path::new(path)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Stage in the target's own directory so the commit is a same-filesystem
/// `rename` — atomic, so no reader ever sees a partial write.
fn stage(target: &Path, bytes: &[u8]) -> Result<std::path::PathBuf, String> {
    let dir = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    let staging = dir.join(format!(".hotl-update.{}.tmp", std::process::id()));

    let write = || -> std::io::Result<()> {
        std::fs::write(&staging, bytes)?;
        copy_mode(target, &staging)
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&staging);
        return Err(unwritable(target, dir, &e));
    }
    Ok(staging)
}

/// Safe on a *running* executable: the live process keeps its inode; the next
/// exec of that path gets the new file.
fn commit(staging: &Path, target: &Path) -> Result<(), String> {
    if let Err(e) = std::fs::rename(staging, target) {
        let _ = std::fs::remove_file(staging);
        let dir = target.parent().unwrap_or(target);
        return Err(unwritable(target, dir, &e));
    }
    Ok(())
}

/// Preserve the old binary's mode (0755 if absent), never widening it — only
/// forcing owner-execute so the replacement is runnable.
fn copy_mode(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(from)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o755);
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(mode | 0o700))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (from, to);
    }
    Ok(())
}

/// A runtime claim, not a configuration one (same discipline as the sandbox
/// probe): a truncated or wrong-arch download is caught here, while the
/// original binary is still in place.
async fn probe(staged: &Path, expect: &str) -> Result<(), String> {
    let run = tokio::process::Command::new(staged)
        .arg("--version")
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .map_err(|_| "the downloaded binary did not respond to `--version`".to_string())?
        .map_err(|e| format!("the downloaded binary would not run: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "the downloaded binary exited {} running `--version`",
            out.status
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.contains(expect) {
        return Err(format!(
            "the downloaded binary reports {:?}, expected {expect}",
            stdout.trim()
        ));
    }
    Ok(())
}

/// Errors-as-prompts: name the directory that needs to be writable, and never
/// imply hotl will escalate on its own.
fn unwritable(target: &Path, dir: &Path, e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    if matches!(e.kind(), PermissionDenied | ReadOnlyFilesystem) {
        format!(
            "cannot replace {} — {} is not writable by you ({e}).\n\
             Re-run with the privileges that own that directory, or reinstall with \
             the installer script.",
            target.display(),
            dir.display()
        )
    } else {
        format!("cannot replace {}: {e}", target.display())
    }
}

// ---------------------------------------------------------------- CLI ------

#[derive(Debug)]
struct Options {
    check_only: bool,
    target_version: Option<String>,
    assume_yes: bool,
}

const USAGE: &str = "usage: hotl update [--check] [--version X.Y.Z] [-y|--yes]

  --check           report whether a newer release exists; never writes
  --version X.Y.Z   install that exact version (allows going back)
  -y, --yes         do not ask before replacing the binary";

impl Options {
    fn parse(rest: &[String]) -> Result<Self, i32> {
        let mut o = Options {
            check_only: false,
            target_version: None,
            assume_yes: false,
        };
        let mut it = rest.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--check" => o.check_only = true,
                "-y" | "--yes" => o.assume_yes = true,
                "-h" | "--help" => {
                    println!("{USAGE}");
                    return Err(0);
                }
                "--version" => match it.next() {
                    Some(v) => o.target_version = Some(v.clone()),
                    None => {
                        eprintln!("hotl update: --version needs a version, e.g. --version 0.8.0");
                        return Err(2);
                    }
                },
                v if v.starts_with("--version=") => {
                    o.target_version = Some(v["--version=".len()..].to_string());
                }
                other => {
                    eprintln!("hotl update: unrecognized argument {other:?}\n{USAGE}");
                    return Err(2);
                }
            }
        }
        Ok(o)
    }
}

/// `banner` is `main`'s version line, passed in so the two can't disagree and
/// so this module stays `crate::`-free for `#[path]` include by a test.
pub async fn update_main(args: &[String], banner: &str) -> i32 {
    let opts = match Options::parse(&args[1..]) {
        Ok(o) => o,
        Err(code) => return code,
    };
    println!("{banner}");
    match run(&opts, RELEASES).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("hotl update: {e}");
            1
        }
    }
}

async fn run(opts: &Options, releases: &str) -> Result<(), String> {
    let current = env!("CARGO_PKG_VERSION");

    let exe =
        std::env::current_exe().map_err(|e| format!("cannot find the running hotl binary: {e}"))?;
    // macOS leaves symlinks unresolved, and `~/.nix-profile/bin` is a symlink
    // chain into the store — classify and write the real file.
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let channel = classify(&exe, cargo_home().as_deref(), crates_toml().as_deref());

    let manifest = fetch_manifest(releases, opts.target_version.as_deref()).await?;

    // --version names the target explicitly, prerelease or older included.
    let latest = match opts.target_version.as_deref() {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => match manifest.upgrade_from(current)? {
            Some(v) => {
                println!("a newer version is available: {v}");
                v
            }
            None => {
                println!("up to date (latest: {}).", manifest.version()?);
                return Ok(());
            }
        },
    };
    if opts.check_only {
        println!("run `hotl update` to install it.");
        return Ok(());
    }

    if hotl_tools::rules::enforced_build() {
        return Err(
            "this is a security-enforced build, and the published binaries are not.\n\
             Replacing it would silently drop the enforced posture. Rebuild instead:\n  \
             cargo install --locked --features security-enforced hotl"
                .to_string(),
        );
    }
    if let Some(advice) = delegate(channel, &exe) {
        println!("{advice}");
        return Ok(());
    }
    let Some(triple) = target_triple() else {
        return Err(format!(
            "no prebuilt binary is published for this platform ({} {}).\n\
             Build from source: cargo install --locked hotl",
            std::env::consts::ARCH,
            std::env::consts::OS
        ));
    };

    let artifact = manifest.artifact_for(triple)?;
    if !opts.assume_yes && !confirm(current, &latest, &exe)? {
        println!("cancelled — nothing was changed.");
        return Ok(());
    }

    println!("downloading {}", artifact.name);
    download_and_install(releases, manifest.tag(), &artifact, &exe, &latest).await?;

    println!(
        "updated {current} -> {latest} at {}\nthe next `hotl` you run is the new one.",
        exe.display()
    );
    Ok(())
}

/// Download, verify, probe, and swap. Takes `target` explicitly so a test can
/// drive the whole pipeline without aiming at its own binary.
pub async fn download_and_install(
    releases: &str,
    tag: &str,
    artifact: &Artifact,
    target: &Path,
    expect_version: &str,
) -> Result<(), String> {
    let url = format!("{releases}/download/{tag}/{}", artifact.name);
    let bytes = get(&url).await?;
    verify_sha256(&bytes, &artifact.sha256)?;
    let new_exe = extract_executable(&bytes, &artifact.exe_path)?;

    let staging = stage(target, &new_exe)?;
    if let Err(e) = probe(&staging, expect_version).await {
        let _ = std::fs::remove_file(&staging);
        return Err(format!("{e}\nNothing was changed."));
    }
    commit(&staging, target)
}

/// Advice for installs hotl must not overwrite. `None` = self-replaceable.
fn delegate(channel: Channel, exe: &Path) -> Option<String> {
    let how = |owner: &str, cmd: &str| {
        Some(format!(
            "installed via: {owner} ({})\n\n\
             hotl does not replace this install. to update:\n\n  {cmd}\n",
            exe.display()
        ))
    };
    match channel {
        Channel::Nix => how("nix", "nix profile upgrade hotl"),
        Channel::Homebrew => how("homebrew", "brew upgrade hotl"),
        Channel::Cargo => how("cargo install", "cargo install --locked hotl"),
        Channel::SourceBuild => how("a source build", "git pull && cargo build --release"),
        Channel::Prebuilt => None,
    }
}

fn confirm(current: &str, latest: &str, exe: &Path) -> Result<bool, String> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "replacing {} needs confirmation, but stdin is not a terminal. \
             Re-run with --yes to proceed without asking.",
            exe.display()
        ));
    }
    print!("replace {} ({current} -> {latest})? [y/N] ", exe.display());
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("could not read your answer: {e}"))?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

async fn fetch_manifest(releases: &str, version: Option<&str>) -> Result<Manifest, String> {
    let url = match version {
        Some(v) => format!(
            "{releases}/download/v{}/dist-manifest.json",
            v.trim_start_matches('v')
        ),
        None => format!("{releases}/latest/download/dist-manifest.json"),
    };
    let body = get(&url).await?;
    Manifest::parse(&String::from_utf8_lossy(&body))
}

/// Explicit user-agent, timeout, and bounded redirects. `Client::new()` would
/// silently discard all three — the trap `web.rs` warns about.
async fn get(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("hotl-update/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(15))
        // `releases/latest/download/...` redirects to the asset host.
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("could not reach {url}: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("no such release ({url})"));
    }
    if !resp.status().is_success() {
        return Err(format!("{url} returned {}", resp.status()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("download from {url} failed: {e}"))
}

fn cargo_home() -> Option<std::path::PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cargo")))
}

fn crates_toml() -> Option<String> {
    std::fs::read_to_string(cargo_home()?.join(".crates.toml")).ok()
}

/// Is `latest` newer than `current`? Unparseable parts count as 0.
///
/// `rank` is 1 for a release, 0 for a prerelease, so `0.8.0-rc.1 < 0.8.0`.
/// rc.1 vs rc.2 is left unordered — the feed never asks for that distinction.
fn is_newer(current: &str, latest: &str) -> bool {
    fn parts(v: &str) -> (u64, u64, u64, u8) {
        let v = v.trim_start_matches('v');
        let rank = u8::from(!v.contains('-'));
        let mut it = v
            .split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            rank,
        )
    }
    parts(latest) > parts(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Trimmed from the real v0.7.1 manifest, extension changed to .tar.gz.
    const MANIFEST: &str = r#"{
      "dist_version": "0.32.0",
      "announcement_tag": "v0.8.0",
      "announcement_is_prerelease": false,
      "artifacts": {
        "hotl-aarch64-apple-darwin.tar.gz": {
          "name": "hotl-aarch64-apple-darwin.tar.gz",
          "kind": "executable-zip",
          "target_triples": ["aarch64-apple-darwin"],
          "assets": [
            { "name": "README.md", "path": "README.md", "kind": "readme" },
            { "name": "hotl", "path": "hotl", "kind": "executable" }
          ],
          "checksum": "hotl-aarch64-apple-darwin.tar.gz.sha256",
          "checksums": { "sha256": "f1e2bce7c8654c359542684720fb2d9f97094bc859058d17e37a3d21f4d0c7a1" }
        },
        "hotl-x86_64-unknown-linux-gnu.tar.gz": {
          "name": "hotl-x86_64-unknown-linux-gnu.tar.gz",
          "kind": "executable-zip",
          "target_triples": ["x86_64-unknown-linux-gnu"],
          "assets": [ { "name": "hotl", "path": "hotl", "kind": "executable" } ],
          "checksums": { "sha256": "aaaa000000000000000000000000000000000000000000000000000000000000" }
        },
        "hotl-installer.sh": {
          "name": "hotl-installer.sh",
          "kind": "installer",
          "target_triples": ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]
        },
        "source.tar.gz": { "name": "source.tar.gz", "kind": "source-tarball" }
      }
    }"#;

    #[test]
    fn reads_the_announced_version_without_the_v() {
        let m = Manifest::parse(MANIFEST).expect("parses");
        assert_eq!(m.version().unwrap(), "0.8.0");
        assert!(!m.announcement_is_prerelease);
    }

    #[test]
    fn selects_the_archive_and_hash_for_a_triple() {
        let m = Manifest::parse(MANIFEST).unwrap();
        let a = m.artifact_for("aarch64-apple-darwin").expect("found");
        assert_eq!(a.name, "hotl-aarch64-apple-darwin.tar.gz");
        assert_eq!(
            a.sha256,
            "f1e2bce7c8654c359542684720fb2d9f97094bc859058d17e37a3d21f4d0c7a1"
        );
        assert_eq!(a.exe_path, "hotl");
    }

    /// The installer shares a triple with the archive; picking by triple alone
    /// would download a shell script and try to run it as the binary.
    #[test]
    fn ignores_artifacts_that_are_not_executable_archives() {
        let m = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(
            m.artifact_for("x86_64-unknown-linux-gnu").unwrap().name,
            "hotl-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn a_triple_with_no_archive_is_an_error() {
        let m = Manifest::parse(MANIFEST).unwrap();
        let e = m.artifact_for("x86_64-pc-windows-msvc").unwrap_err();
        assert!(
            e.contains("x86_64-pc-windows-msvc"),
            "names the triple: {e}"
        );
    }

    /// Every release through v0.7.1 shipped `.tar.xz`, which this build cannot
    /// decode. `--version 0.7.0` must say so rather than fail inside the gzip
    /// reader with a decode error.
    #[test]
    fn a_pre_gzip_release_is_refused_by_name() {
        let xz = MANIFEST.replace(".tar.gz", ".tar.xz");
        let m = Manifest::parse(&xz).unwrap();
        let e = m.artifact_for("aarch64-apple-darwin").unwrap_err();
        assert!(e.contains("predates"), "explains the format gap: {e}");
    }

    #[test]
    fn a_manifest_with_no_tag_is_an_error() {
        let m = Manifest::parse(r#"{"artifacts":{}}"#).unwrap();
        assert!(m.version().is_err());
    }

    /// Flat `.tar.gz`, as the release pipeline emits (exe at archive root).
    fn targz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, path, *body).unwrap();
        }
        gzip(&tar.into_inner().unwrap())
    }

    fn gzip(raw: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut enc, raw).unwrap();
        enc.finish().unwrap()
    }

    /// Hand-built ustar so the entry name can be anything — `tar::Builder`
    /// validates paths and would refuse.
    fn raw_tar_entry(name: &str, body: &[u8]) -> Vec<u8> {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..108].copy_from_slice(b"0000755\0"); // mode
        h[108..116].copy_from_slice(b"0000000\0"); // uid
        h[116..124].copy_from_slice(b"0000000\0"); // gid
        h[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
        h[136..148].copy_from_slice(b"00000000000\0"); // mtime
        h[148..156].copy_from_slice(b"        "); // checksum field reads as spaces while summing
        h[156] = b'0'; // typeflag: regular file
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());

        let mut out = h.to_vec();
        out.extend_from_slice(body);
        out.resize(out.len() + (512 - body.len() % 512) % 512, 0);
        out.resize(out.len() + 1024, 0); // two zero blocks terminate the archive
        out
    }

    fn sha256_of(bytes: &[u8]) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(bytes))
    }

    #[test]
    fn verify_accepts_the_matching_hash_and_rejects_anything_else() {
        let bytes = b"an archive";
        assert!(verify_sha256(bytes, &sha256_of(bytes)).is_ok());

        let wrong = "0".repeat(64);
        let e = verify_sha256(bytes, &wrong).unwrap_err();
        assert!(e.contains("checksum"), "says what failed: {e}");

        // Case must not matter; the manifest is lowercase but that is not a
        // guarantee worth depending on.
        assert!(verify_sha256(bytes, &sha256_of(bytes).to_uppercase()).is_ok());
    }

    #[test]
    fn extracts_the_named_executable_from_the_archive() {
        let gz = targz(&[
            ("README.md", b"docs"),
            ("hotl", b"#!/bin/sh\necho new binary\n"),
        ]);
        let got = extract_executable(&gz, "hotl").expect("extracts");
        assert_eq!(got, b"#!/bin/sh\necho new binary\n");
    }

    #[test]
    fn an_archive_without_the_executable_is_an_error() {
        let gz = targz(&[("README.md", b"docs")]);
        let e = extract_executable(&gz, "hotl").unwrap_err();
        assert!(e.contains("hotl"), "names what was missing: {e}");
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.1.2", "0.2.0"));
        assert!(is_newer("0.1.2", "0.1.3"));
        assert!(is_newer("v0.1.2", "v1.0.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.2", "0.1.2"));
    }

    /// Without the rank, 0.8.0-rc.1 and 0.8.0 compare equal and the real
    /// release reads as "up to date".
    #[test]
    fn prerelease_sorts_below_its_release() {
        assert!(is_newer("0.8.0-rc.1", "0.8.0"));
        assert!(!is_newer("0.8.0", "0.8.0-rc.1"));
        assert!(!is_newer("0.8.0-rc.1", "0.8.0-rc.1"));
        // A prerelease of a *later* version still wins.
        assert!(is_newer("0.7.1", "0.8.0-rc.1"));
    }

    /// A plain `hotl update` must never land someone on a prerelease.
    #[test]
    fn a_prerelease_is_never_offered_to_a_plain_update() {
        let pre = MANIFEST.replace("\"v0.8.0\"", "\"v0.9.0-rc.1\"").replace(
            "\"announcement_is_prerelease\": false",
            "\"announcement_is_prerelease\": true",
        );
        let m = Manifest::parse(&pre).unwrap();
        assert_eq!(m.version().unwrap(), "0.9.0-rc.1");
        assert_eq!(
            m.upgrade_from("0.7.1").unwrap(),
            None,
            "a prerelease is newer, but must not be offered"
        );

        // The same manifest marked as a real release is offered.
        let m = Manifest::parse(&pre.replace(
            "\"announcement_is_prerelease\": true",
            "\"announcement_is_prerelease\": false",
        ))
        .unwrap();
        assert_eq!(
            m.upgrade_from("0.7.1").unwrap().as_deref(),
            Some("0.9.0-rc.1")
        );
    }

    #[test]
    fn an_equal_or_older_release_is_not_an_upgrade() {
        let m = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(m.upgrade_from("0.8.0").unwrap(), None, "same version");
        assert_eq!(m.upgrade_from("0.9.0").unwrap(), None, "already ahead");
        assert_eq!(m.upgrade_from("0.7.1").unwrap().as_deref(), Some("0.8.0"));
    }

    #[test]
    fn flags_parse_and_bad_ones_are_rejected() {
        let o = Options::parse(&["--check".into()]).unwrap();
        assert!(o.check_only && !o.assume_yes && o.target_version.is_none());

        let o = Options::parse(&["--version".into(), "0.8.0".into(), "-y".into()]).unwrap();
        assert_eq!(o.target_version.as_deref(), Some("0.8.0"));
        assert!(o.assume_yes);

        assert_eq!(
            Options::parse(&["--version=0.8.0".into()])
                .unwrap()
                .target_version
                .as_deref(),
            Some("0.8.0")
        );
        // `--version` with nothing after it is a usage error, not a silent no-op.
        assert_eq!(Options::parse(&["--version".into()]).unwrap_err(), 2);
        assert_eq!(Options::parse(&["--nope".into()]).unwrap_err(), 2);
    }

    /// Every managed channel must yield advice, never fall through to a write.
    #[test]
    fn managed_channels_delegate_with_the_right_command() {
        let exe = Path::new("/somewhere/hotl");
        for (channel, expected) in [
            (Channel::Nix, "nix profile upgrade hotl"),
            (Channel::Homebrew, "brew upgrade hotl"),
            (Channel::Cargo, "cargo install --locked hotl"),
            (Channel::SourceBuild, "cargo build --release"),
        ] {
            let advice = delegate(channel, exe)
                .unwrap_or_else(|| panic!("{channel:?} must not self-replace"));
            assert!(advice.contains(expected), "{channel:?}: {advice}");
        }
        assert!(
            delegate(Channel::Prebuilt, exe).is_none(),
            "prebuilt installs are the ones hotl may replace"
        );
    }

    #[test]
    fn unsafe_entry_paths_are_recognised() {
        for good in ["hotl", "bin/hotl", "a/b/c"] {
            assert!(is_safe_entry_path(good), "{good} should be allowed");
        }
        for bad in ["../evil", "/etc/passwd", "a/../../evil", "", "/"] {
            assert!(!is_safe_entry_path(bad), "{bad:?} should be refused");
        }
    }

    /// `exe_path` comes from the manifest — attacker-influenced in exactly the
    /// scenario the checksum cannot cover.
    #[test]
    fn a_manifest_naming_an_escaping_executable_is_refused() {
        let gz = targz(&[("hotl", b"body")]);
        let e = extract_executable(&gz, "../../.ssh/authorized_keys").unwrap_err();
        assert!(
            e.contains("unsafe path"),
            "refused for the right reason: {e}"
        );
    }

    /// Guards archives our own pipeline did not produce.
    #[test]
    fn an_archive_containing_a_traversing_entry_is_refused() {
        let gz = gzip(&raw_tar_entry("../evil", b"payload"));
        let e = extract_executable(&gz, "hotl").unwrap_err();
        assert!(
            e.contains("unsafe path"),
            "refused for the right reason: {e}"
        );
    }

    #[test]
    fn swap_replaces_the_target_atomically_and_preserves_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hotl");
        std::fs::write(&target, b"old binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let staged = stage(&target, b"new binary").expect("stages");
        commit(&staged, &target).expect("commits");

        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "mode preserved, got {mode:o}");
        }
        // No staging file may survive a successful swap.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "hotl")
            .collect();
        assert!(leftovers.is_empty(), "left staging files: {leftovers:?}");
    }

    /// Quietly widening a deliberately-0700 binary would be a security bug.
    #[cfg(unix)]
    #[test]
    fn a_restrictive_mode_is_not_widened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hotl");
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();

        let staged = stage(&target, b"new").unwrap();
        commit(&staged, &target).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "not widened, got {mode:o}");
    }

    /// The probe runs between stage and commit; a bad replacement must leave
    /// the original in place.
    #[test]
    fn a_staged_file_discarded_before_commit_leaves_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hotl");
        std::fs::write(&target, b"old binary").unwrap();

        let staged = stage(&target, b"new binary").unwrap();
        assert!(staged.exists());
        std::fs::remove_file(&staged).unwrap(); // stands in for a failed probe

        assert_eq!(std::fs::read(&target).unwrap(), b"old binary");
    }

    #[test]
    fn staging_into_a_missing_directory_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("hotl");
        // Parent does not exist, so staging cannot be created.
        assert!(stage(&target, b"new binary").is_err());
        assert!(!target.exists());
    }

    const CRATES_TOML_WITH_HOTL: &str = r#"[v1]
"hotl 0.7.1 (registry+https://github.com/rust-lang/crates.io-index)" = ["hotl"]
"ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)" = ["rg"]
"#;

    const CRATES_TOML_WITHOUT_HOTL: &str = r#"[v1]
"ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)" = ["rg"]
"#;

    fn cargo_home() -> PathBuf {
        PathBuf::from("/home/u/.cargo")
    }

    #[test]
    fn nix_store_is_never_writable() {
        assert_eq!(
            classify(
                Path::new("/nix/store/1a2b3c-hotl-0.7.1/bin/hotl"),
                Some(&cargo_home()),
                None
            ),
            Channel::Nix
        );
    }

    #[test]
    fn target_dir_is_a_source_build() {
        assert_eq!(
            classify(
                Path::new("/home/u/src/hotl/target/release/hotl"),
                None,
                None
            ),
            Channel::SourceBuild
        );
        assert_eq!(
            classify(Path::new("/home/u/src/hotl/target/debug/hotl"), None, None),
            Channel::SourceBuild
        );
    }

    #[test]
    fn cellar_path_is_homebrew() {
        assert_eq!(
            classify(
                Path::new("/opt/homebrew/Cellar/hotl/0.7.1/bin/hotl"),
                None,
                None
            ),
            Channel::Homebrew
        );
    }

    /// Load-bearing: `install-path = "CARGO_HOME"` puts the dist installer and
    /// `cargo install` in the same dir, so only `.crates.toml` separates them.
    #[test]
    fn cargo_bin_splits_on_the_crates_toml_record() {
        let exe = PathBuf::from("/home/u/.cargo/bin/hotl");
        assert_eq!(
            classify(&exe, Some(&cargo_home()), Some(CRATES_TOML_WITH_HOTL)),
            Channel::Cargo,
            "a .crates.toml record means cargo owns this file"
        );
        assert_eq!(
            classify(&exe, Some(&cargo_home()), Some(CRATES_TOML_WITHOUT_HOTL)),
            Channel::Prebuilt,
            "cargo tracks other crates but not hotl -> the installer put it here"
        );
        assert_eq!(
            classify(&exe, Some(&cargo_home()), None),
            Channel::Prebuilt,
            "no .crates.toml at all -> the installer put it here"
        );
    }

    #[test]
    fn anywhere_else_is_prebuilt() {
        assert_eq!(
            classify(
                Path::new("/home/u/.local/bin/hotl"),
                Some(&cargo_home()),
                Some(CRATES_TOML_WITH_HOTL)
            ),
            Channel::Prebuilt,
            "a cargo record must not claim a binary outside cargo's bin dir"
        );
        assert_eq!(
            classify(Path::new("/usr/local/bin/hotl"), None, None),
            Channel::Prebuilt
        );
    }
}
