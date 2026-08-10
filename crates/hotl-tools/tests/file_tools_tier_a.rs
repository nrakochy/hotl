//! Plan 0022 task 5: the kernel carve confines child processes, but the hotl
//! process is not sandboxed — so the in-process file tools enforce Tier A
//! themselves. Its own test binary because `EXTRAS` is process-wide and
//! set-once, and the rest of the suite asserts an *empty* carve.

use hotl_tools::sandbox::{self, FileToolsMode, ReadDeny, SandboxExtras};
use hotl_tools::{EditTool, Permission, ReadTool, Tool, WriteTool};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const CANARY: &str = "hotl-token-canary-4b71";

#[tokio::test]
async fn the_file_tools_refuse_hotls_own_dirs_outright() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let run_dir = scratch.path().join("data-hotl").join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let token = run_dir.join("session.token");
    std::fs::write(&token, CANARY).unwrap();
    // A symlink *outside* Tier A whose target is inside it: the classification
    // has to run on the resolved path or this laundering works.
    let laundered = scratch.path().join("innocent");
    std::os::unix::fs::symlink(&run_dir, &laundered).unwrap();

    sandbox::init_extras(SandboxExtras {
        writable: Vec::new(),
        file_tools: FileToolsMode::Workspace,
        read_deny: ReadDeny {
            always: vec![dunce::canonicalize(&run_dir).unwrap()],
            secrets: Vec::new(),
            rules: Vec::new(),
        },
    });

    let read = ReadTool::default();
    let cancel = CancellationToken::new;

    for path in [
        token.display().to_string(),
        laundered.join("session.token").display().to_string(),
        // A `..` walk back into the carve, never canonicalized because the
        // target need not exist for `write`.
        run_dir
            .join("..")
            .join("run")
            .join("session.token")
            .display()
            .to_string(),
    ] {
        let input = json!({ "path": path });
        // The ask names the reason; the human is told what was attempted.
        match read.permission(&input) {
            Permission::AskProtected { why, .. } => {
                assert!(why.contains("hotl's own"), "{why}");
                assert!(why.contains("refused outright"), "{why}");
            }
            other => panic!("`{path}` must be protected, got {other:?}"),
        }
        // And the run-time door refuses regardless of what was approved —
        // this is what makes it unaskable rather than merely escalated.
        let outcome = read.run(input, cancel()).await;
        assert!(outcome.is_error, "`{path}` must be refused");
        assert!(
            !outcome.content.contains(CANARY),
            "the token leaked into the tool result: {}",
            outcome.content
        );
    }

    // Positive control: an ordinary file beside the carve reads fine, so the
    // refusals above are the carve and not a broken `read`.
    let ordinary = scratch.path().join("notes.txt");
    std::fs::write(&ordinary, "hello").unwrap();
    let ok = read
        .run(json!({ "path": ordinary.display().to_string() }), cancel())
        .await;
    assert!(!ok.is_error, "{}", ok.content);
    assert!(ok.content.contains("hello"));

    // The write side of the same escalation: rewriting hotl's allow-rules or
    // hooks is at least as bad as reading the token.
    let write = WriteTool::default();
    let planted = write
        .run(
            json!({ "path": run_dir.join("hook.sh").display().to_string(), "content": "x" }),
            cancel(),
        )
        .await;
    assert!(planted.is_error, "{}", planted.content);
    assert!(!run_dir.join("hook.sh").exists(), "the write must not land");

    let edited = EditTool::default()
        .run(
            json!({
                "path": token.display().to_string(),
                "old_string": CANARY,
                "new_string": "stolen",
            }),
            cancel(),
        )
        .await;
    assert!(edited.is_error, "{}", edited.content);
    assert_eq!(std::fs::read_to_string(&token).unwrap(), CANARY);
}
