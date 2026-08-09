//! End-to-end proof of the `[sandbox].writable` widening through the real
//! process-global path: `init_extras` → `probe()` → `build_command`. Its own
//! test binary because `EXTRAS` and the probe verdict are process-wide
//! (set-once / memoized), so no other test may share the process — and a
//! single test fn, so nothing races the init-before-probe ordering the
//! production wiring promises.

use hotl_tools::net::EgressState;
use hotl_tools::sandbox::{self, FileToolsMode, SandboxExtras, SandboxStatus};

#[tokio::test]
async fn configured_extras_widen_the_floor_and_keep_the_probe_sound() {
    // An extra outside the default floor (a tempdir would sit under TMPDIR,
    // which is already writable — vacuous). $HOME is the documented probe
    // candidate, so a subdir of it is guaranteed outside cwd/TMPDIR here.
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let extra = home.join(format!(".hotl-extras-itest-{}", std::process::id()));
    std::fs::create_dir_all(&extra).unwrap();

    sandbox::init_extras(SandboxExtras {
        writable: vec![extra.clone()],
        file_tools: FileToolsMode::Workspace,
        // The read-carve has its own integration binary (`sandbox_read_carve`);
        // an empty set here keeps this test about the write widening alone.
        read_deny: sandbox::ReadDeny::default(),
    });

    let status = sandbox::probe();
    let SandboxStatus::Enforced(_) = &status else {
        // No floor on this host: the widening has nothing to widen. The
        // verdict/host agreement is sandbox_probe.rs's job; here only assert
        // the probe still fails loudly rather than certifying nonsense.
        std::fs::remove_dir_all(&extra).ok();
        assert!(!matches!(status, SandboxStatus::Enforced(_)));
        return;
    };

    // The probe's positive control must have avoided the configured extra —
    // a target inside it would "prove" confinement that was never tested.
    let probe_dir = sandbox::probe_dir().expect("a probe dir outside the widened floor");
    assert!(
        !probe_dir.starts_with(&extra),
        "probe target {} sits inside the configured extra {}",
        probe_dir.display(),
        extra.display()
    );

    // A write under the extra is allowed, through the real spawn path.
    let inside = extra.join("cache-file");
    let ok = sandbox::build_command(
        &format!("echo x > {}", inside.display()),
        &status,
        &EgressState::Open,
    )
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .output()
    .await
    .expect("spawn");
    let landed = std::fs::read_to_string(&inside).unwrap_or_default();

    // A write beside the extra (outside every root) still fails, and leaves
    // nothing on disk.
    let outside = home.join(format!(".hotl-extras-denied-{}", std::process::id()));
    let denied = sandbox::build_command(
        &format!("echo x > {}", outside.display()),
        &status,
        &EgressState::Open,
    )
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .output()
    .await
    .expect("spawn");
    let leaked = outside.exists();
    if leaked {
        std::fs::remove_file(&outside).ok();
    }
    std::fs::remove_dir_all(&extra).ok();

    assert!(
        ok.status.success(),
        "write under the configured extra must be allowed: {}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(landed.trim(), "x");
    assert!(
        !denied.status.success(),
        "write outside the widened floor must still be denied"
    );
    assert!(!leaked, "no file may land outside the widened floor");
}
