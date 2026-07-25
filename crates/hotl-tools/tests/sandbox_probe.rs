//! D-9: `Enforced` must mean "a sandboxed child failed to escape on this
//! host", never "the mechanism's binary exists on disk". Its own test binary
//! because it spawns processes and writes outside the workspace.

use hotl_tools::sandbox::{self, SandboxStatus};

#[test]
fn probe_refuses_a_mechanism_that_does_not_confine() {
    // A "mechanism" that is really just `sh -c` — it confines nothing.
    let result = sandbox::verify_confinement_with("fake", |script| {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    });
    let reason = result.expect_err("an unconfined child must not certify a mechanism");
    assert!(
        reason.contains("fake") && reason.contains("did not confine"),
        "the reason must name the mechanism and what it failed to do: {reason}"
    );
}

#[test]
fn probe_is_memoized_so_it_costs_one_spawn_per_process() {
    let first = sandbox::probe();
    let t = std::time::Instant::now();
    for _ in 0..50 {
        assert_eq!(sandbox::probe(), first);
    }
    // 50 real spawns cannot finish in 50ms; a memoized read trivially can.
    assert!(
        t.elapsed() < std::time::Duration::from_millis(50),
        "probe re-spawned"
    );
}

/// The positive direction, only where a floor genuinely exists. Not
/// `return`-on-absent (§8's vacuous-pass pattern): the assertion is that the
/// *verdict matches the host*, which is checkable either way.
#[test]
fn probe_certifies_a_host_that_really_confines() {
    match sandbox::probe() {
        SandboxStatus::Enforced(m) => {
            assert!(
                matches!(m, "seatbelt" | "landlock" | "landlock(partial)"),
                "{m}"
            );
            // Certification implies the smoke test passed just now; re-running
            // the real builder must agree.
            assert!(sandbox::verify_confinement_with(m, |script| {
                sandbox::build_command(
                    script,
                    &SandboxStatus::Enforced(m),
                    &hotl_tools::net::EgressState::Open,
                )
            })
            .is_ok());
        }
        SandboxStatus::Unavailable(reason) => {
            assert!(!reason.is_empty(), "an Unavailable verdict must say why");
        }
        SandboxStatus::Disabled => assert!(std::env::var("HOTL_SANDBOX").is_ok()),
    }
}
