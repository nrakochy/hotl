//! End-to-end proof of the plan-0022 read-carve through the real
//! process-global path: `init_extras` → `probe()` → `build_command`. Its own
//! test binary because `EXTRAS` and the probe verdict are process-wide
//! (set-once / memoized), so no other test may share the process — and a
//! single test fn, so nothing races the init-before-probe ordering the
//! production wiring promises.

use hotl_tools::net::EgressState;
use hotl_tools::sandbox::{self, FileToolsMode, ReadDeny, SandboxExtras, SandboxStatus};

async fn run(script: &str, status: &SandboxStatus) -> std::process::Output {
    sandbox::build_command(script, status, &EgressState::Open)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .expect("spawn")
}

#[tokio::test]
async fn the_carve_denies_hotls_own_run_dir_through_the_process_global_path() {
    // A stand-in for `<data_dir>/hotl/run` holding a stand-in session token,
    // and an ordinary sibling that must stay readable. Both live outside the
    // write floor, or the carve could not be honored at all (a write grant on
    // an ancestor re-opens the read — see config.rs::resolve_read_deny).
    let base = sandbox::probe_dir().expect("a dir outside the floor");
    let run_dir = base.join(format!("hotl-carve-run-{}", std::process::id()));
    let ordinary = base.join(format!("hotl-carve-open-{}", std::process::id()));
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::create_dir_all(&ordinary).unwrap();
    let token = run_dir.join("session.token");
    let sibling = ordinary.join("session.token");
    const CANARY: &str = "hotl-canary-9f3c1d";
    std::fs::write(&token, CANARY).unwrap();
    std::fs::write(&sibling, CANARY).unwrap();

    sandbox::init_extras(SandboxExtras {
        writable: Vec::new(),
        file_tools: FileToolsMode::Workspace,
        read_deny: ReadDeny {
            always: vec![run_dir.clone()],
            secrets: Vec::new(),
        },
    });

    let status = sandbox::probe();
    let cleanup = || {
        std::fs::remove_dir_all(&run_dir).ok();
        std::fs::remove_dir_all(&ordinary).ok();
    };
    let SandboxStatus::Enforced(_) = &status else {
        // No floor on this host: nothing to carve. The verdict/host agreement
        // is sandbox_probe.rs's job; here only assert the probe still fails
        // loudly rather than certifying a carve it never applied.
        cleanup();
        assert!(!matches!(status, SandboxStatus::Enforced(_)));
        return;
    };

    // Plan 0022 task 7: the probe tested the carve in the same spawn it uses
    // for the write, and certified it. It also cleaned up after itself — a
    // leaked probe canary inside Tier A is exactly the thing the carve exists
    // to keep out of reach.
    assert_eq!(
        sandbox::read_carve_enforced(),
        Some(true),
        "the probe must certify the read carve on a host that enforces it"
    );
    let strays: Vec<_> = std::fs::read_dir(&run_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "session.token")
        .collect();
    assert!(strays.is_empty(), "the probe left {strays:?} behind");

    // Positive control: the canary is readable from the parent, and from a
    // sandboxed child at a path outside the carve. Without this the negative
    // below would pass on a host where nothing is readable anyway.
    assert_eq!(std::fs::read_to_string(&token).unwrap(), CANARY);
    let control = run(&format!("cat {}", sibling.display()), &status).await;

    let denied = run(&format!("cat {}", token.display()), &status).await;
    // The path still *exists* — the carve denies contents, not metadata, so
    // `ls -la` keeps working (macOS `file-read-data`, Linux ancestor ReadDir).
    let stat = run(&format!("stat {} > /dev/null", token.display()), &status).await;
    // A directory listing of the carve's parent still works.
    let list = run(&format!("ls {} > /dev/null", base.display()), &status).await;
    cleanup();

    assert!(
        control.status.success() && String::from_utf8_lossy(&control.stdout).contains(CANARY),
        "the control canary must read from a sandboxed child: {}",
        String::from_utf8_lossy(&control.stderr)
    );
    assert!(
        !denied.status.success(),
        "reading the carved token must fail"
    );
    // The canary reaches neither the model's view of stdout nor stderr.
    let seen = format!(
        "{}{}",
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(
        !seen.contains(CANARY),
        "the canary leaked into output: {seen}"
    );
    assert!(
        stat.status.success(),
        "metadata must stay readable: {}",
        String::from_utf8_lossy(&stat.stderr)
    );
    assert!(
        list.status.success(),
        "listing the carve's parent must still work: {}",
        String::from_utf8_lossy(&list.stderr)
    );
}
