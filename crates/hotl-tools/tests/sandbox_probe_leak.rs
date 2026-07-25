//! D-9, cleanup half: the verifier writes a real file *outside* the
//! confinement, so on the escape path it must delete what leaked.
//!
//! Its own test binary, and the filename filter is scoped to this process's
//! pid, because the probe file name is `hotl-sbx-probe-<pid>-<nanos>`: any
//! sibling test that also calls `probe()`/`verify_confinement_with` shares the
//! pid and the directory, so a before/after count in a shared binary races
//! against them and fails intermittently. One test, one process, one pid makes
//! the count deterministic.

use hotl_tools::sandbox;

#[test]
fn probe_leaves_no_file_behind_when_the_write_escapes() {
    let dir = sandbox::probe_dir().expect("a probe dir");
    let prefix = format!("hotl-sbx-probe-{}-", std::process::id());
    let count = |dir: &std::path::Path| {
        std::fs::read_dir(dir)
            .map(|it| {
                it.flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                    .count()
            })
            .unwrap_or(0)
    };
    let start = count(&dir);
    // `sh -c` confines nothing, so the write lands — the escape path.
    let result = sandbox::verify_confinement_with("fake", |script| {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    });
    assert!(
        result.is_err(),
        "the unconfined write must be the escape path, or this proves nothing"
    );
    assert_eq!(
        count(&dir),
        start,
        "the leaked probe file must be cleaned up"
    );
}
