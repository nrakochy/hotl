//! End-to-end `hotl update` install against a local server: fetch, verify,
//! extract, probe, swap. No network, no real release.

#[path = "../src/update.rs"]
#[allow(dead_code)]
mod update;

use std::io::Write as _;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Flat `.tar.gz` shaped like a release archive: the executable at the root.
fn targz(exe_body: &str) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let body = exe_body.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, "hotl", body).unwrap();

    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(&tar.into_inner().unwrap()).unwrap();
    enc.finish().unwrap()
}

fn sha256_of(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Serve `body` for any GET, once per connection. Returns the base URL.
async fn serve(body: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.flush().await;
            });
        }
    });
    format!("http://{addr}")
}

/// A stand-in for the real binary: it answers `--version` like hotl does.
fn fake_binary(version: &str) -> String {
    format!("#!/bin/sh\necho \"hotl {version}\"\n")
}

fn existing_binary(dir: &Path) -> std::path::PathBuf {
    let target = dir.join("hotl");
    std::fs::write(&target, "#!/bin/sh\necho \"hotl 0.7.1\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    target
}

#[tokio::test]
async fn downloads_verifies_and_replaces_the_binary() {
    let archive = targz(&fake_binary("0.8.0"));
    let artifact = update::Artifact {
        name: "hotl-x86_64-unknown-linux-gnu.tar.gz".into(),
        sha256: sha256_of(&archive),
        exe_path: "hotl".into(),
    };
    let base = serve(archive).await;

    let dir = tempfile::tempdir().unwrap();
    let target = existing_binary(dir.path());

    update::download_and_install(&base, "v0.8.0", &artifact, &target, "0.8.0")
        .await
        .expect("installs");

    let installed = std::fs::read_to_string(&target).unwrap();
    assert!(
        installed.contains("0.8.0"),
        "binary replaced: {installed:?}"
    );

    // The replacement is runnable, which is what the probe asserted before the swap.
    let out = std::process::Command::new(&target)
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hotl 0.8.0");

    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "hotl")
        .collect();
    assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
}

/// The checksum is the only thing standing between a corrupted download and
/// the binary on your PATH.
#[tokio::test]
async fn a_corrupted_download_leaves_the_original_in_place() {
    let archive = targz(&fake_binary("0.8.0"));
    let artifact = update::Artifact {
        name: "hotl-x86_64-unknown-linux-gnu.tar.gz".into(),
        sha256: sha256_of(&archive),
        exe_path: "hotl".into(),
    };
    // Serve something else entirely; the advertised hash no longer matches.
    let base = serve(b"not the archive you were promised".to_vec()).await;

    let dir = tempfile::tempdir().unwrap();
    let target = existing_binary(dir.path());

    let err = update::download_and_install(&base, "v0.8.0", &artifact, &target, "0.8.0")
        .await
        .expect_err("must refuse");
    assert!(
        err.contains("checksum"),
        "refused for the right reason: {err}"
    );

    assert!(
        std::fs::read_to_string(&target).unwrap().contains("0.7.1"),
        "the original binary must survive a failed update"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

/// A download that passes its checksum but cannot run must not be installed.
#[tokio::test]
async fn a_binary_that_fails_its_probe_is_not_installed() {
    let archive = targz("#!/bin/sh\nexit 3\n");
    let artifact = update::Artifact {
        name: "hotl-x86_64-unknown-linux-gnu.tar.gz".into(),
        sha256: sha256_of(&archive),
        exe_path: "hotl".into(),
    };
    let base = serve(archive).await;

    let dir = tempfile::tempdir().unwrap();
    let target = existing_binary(dir.path());

    let err = update::download_and_install(&base, "v0.8.0", &artifact, &target, "0.8.0")
        .await
        .expect_err("must refuse");
    assert!(
        err.contains("Nothing was changed"),
        "says so plainly: {err}"
    );

    assert!(std::fs::read_to_string(&target).unwrap().contains("0.7.1"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
