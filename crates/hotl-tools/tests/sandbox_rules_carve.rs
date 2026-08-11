//! Plan 0025's reason to exist: a projectable path `[[deny]]` reaches shell
//! commands, not just hotl's own file tools.
//!
//! The whole path in one test: `Rules::from_toml` → `rules::project` →
//! `init_extras` → `probe()` → `build_command`. Its own test binary because
//! `EXTRAS` and the probe verdict are process-wide (set-once / memoized), so no
//! other test may share the process, and one test fn so nothing races the
//! init-before-probe ordering the production wiring promises.
#![cfg(unix)] // the sandbox carve (Landlock / sandbox-exec) is unix-only

use hotl_tools::net::EgressState;
use hotl_tools::rules::{self, Rules};
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
async fn a_projected_deny_rule_denies_reads_to_a_sandboxed_command() {
    // Outside the write floor, or the carve could not be honored at all: a
    // write grant on an ancestor re-opens the read (Landlock unions rights
    // across ancestors — measured in 0022, tracker #51). A tempdir would sit
    // inside TMPDIR and the denial would silently not be delivered.
    let base = sandbox::probe_dir().expect("a dir outside the floor");
    let vault = base.join(format!("hotl-0025-vault-{}", std::process::id()));
    let ordinary = base.join(format!("hotl-0025-open-{}", std::process::id()));
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::create_dir_all(&ordinary).unwrap();
    let secret = vault.join("k");
    let sibling = ordinary.join("k");
    const CANARY: &str = "hotl-canary-0025-4b71e2";
    std::fs::write(&secret, CANARY).unwrap();
    std::fs::write(&sibling, CANARY).unwrap();

    // Deliberately NOT `~/.ssh`: that is already denied by 0022's Tier B, so a
    // test against it would pass without this plan and prove nothing.
    let rules = Rules::from_toml(&format!(
        "[[deny]]\ntool = \"read\"\npath_prefix = \"{}\"\n",
        vault.display()
    ))
    .unwrap();
    let containment = rules::project(&rules, &|p| dunce::canonicalize(p).ok());
    assert_eq!(
        containment.paths(),
        vec![dunce::canonicalize(&vault).unwrap()],
        "the rule must project: {:#?}",
        containment.unprojectable
    );

    // The same rule written `~/…`, expanding against a home the test controls,
    // projects to the same path.
    let tilde = Rules::from_toml(&format!(
        "[[deny]]\ntool = \"read\"\npath_prefix = \"~/{}\"\n",
        vault.file_name().unwrap().to_string_lossy()
    ))
    .unwrap()
    .with_home(Some(base.clone()));
    assert_eq!(
        rules::project(&tilde, &|p| dunce::canonicalize(p).ok()).paths(),
        containment.paths(),
        "the ~/-rooted form must project to the same path"
    );

    sandbox::init_extras(SandboxExtras {
        writable: Vec::new(),
        file_tools: FileToolsMode::Workspace,
        read_deny: ReadDeny {
            always: Vec::new(),
            secrets: Vec::new(),
            rules: containment.paths(),
        },
    });

    // The projection reached the process-global extras, in its own tier.
    assert_eq!(sandbox::read_deny().rules, containment.paths());
    assert!(sandbox::read_deny().always.is_empty());

    let status = sandbox::probe();
    let cleanup = || {
        std::fs::remove_dir_all(&vault).ok();
        std::fs::remove_dir_all(&ordinary).ok();
    };
    let SandboxStatus::Enforced(_) = &status else {
        // No floor on this host: nothing to carve, and the verdict/host
        // agreement is sandbox_probe.rs's job.
        cleanup();
        assert!(!matches!(status, SandboxStatus::Enforced(_)));
        return;
    };

    // Positive control first: without it the denial below would pass on a host
    // where nothing is readable anyway (tests/sandbox_probe.rs:35-38).
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), CANARY);
    let control = run(&format!("cat {}", sibling.display()), &status).await;
    let denied = run(&format!("cat {}", secret.display()), &status).await;
    cleanup();

    assert!(
        control.status.success() && String::from_utf8_lossy(&control.stdout).contains(CANARY),
        "a sibling outside the deny must still read: {}",
        String::from_utf8_lossy(&control.stderr)
    );
    assert!(
        !denied.status.success(),
        "a projected [[deny]] must stop `cat` in a sandboxed child"
    );
    let seen = format!(
        "{}{}",
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(
        !seen.contains(CANARY),
        "the canary leaked into output: {seen}"
    );
}
