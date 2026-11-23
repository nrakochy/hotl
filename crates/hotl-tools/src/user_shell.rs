use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::builtins::{
    adopt, clip_output, collect_merged_into, kill_tree, merged_pipe, new_reaper, BASH_MAX_OUTPUT,
    BASH_OUTPUT_SLACK,
};

pub const USER_SHELL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserShellRun {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl UserShellRun {
    fn failed(output: String) -> Self {
        Self {
            output,
            exit_code: None,
            timed_out: false,
            cancelled: false,
        }
    }
}

pub async fn run(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    cancel: CancellationToken,
) -> UserShellRun {
    let shell = match crate::shell::resolve_shell() {
        Ok(s) => s,
        Err(reason) => return UserShellRun::failed(reason.to_string()),
    };
    let mut cmd = tokio::process::Command::new(&shell.path);
    cmd.arg("-c").arg(command);
    cmd.current_dir(cwd);
    cmd.stdin(std::process::Stdio::null());
    crate::sandbox::detach_session(&mut cmd);
    let (child_out, child_err, pipe) = match merged_pipe() {
        Ok(p) => p,
        Err(e) => return UserShellRun::failed(format!("Could not create the output pipe: {e}.")),
    };
    cmd.stdout(child_out).stderr(child_err).kill_on_drop(true);
    let reaper = new_reaper();
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return UserShellRun::failed(format!("Could not start shell: {e}.")),
    };
    adopt(&reaper, &child);
    drop(cmd);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let wait = collect_merged_into(
        child,
        pipe,
        BASH_MAX_OUTPUT + BASH_OUTPUT_SLACK,
        None,
        buf.clone(),
    );
    tokio::pin!(wait);
    let (timed_out, cancelled) = tokio::select! {
        result = &mut wait => return match result {
            Ok(status) => UserShellRun {
                output: clip_output(&take(&buf)),
                exit_code: status.code(),
                timed_out: false,
                cancelled: false,
            },
            Err(e) => UserShellRun::failed(format!("Failed waiting on command: {e}.")),
        },
        _ = tokio::time::sleep(timeout) => (true, false),
        _ = cancel.cancelled() => (false, true),
    };
    kill_tree(&reaper);
    let _ = tokio::time::timeout(KILLED_DRAIN_GRACE, &mut wait).await;
    UserShellRun {
        timed_out,
        cancelled,
        ..UserShellRun::failed(clip_output(&take(&buf)))
    }
}

const KILLED_DRAIN_GRACE: Duration = Duration::from_millis(500);

fn take(buf: &Mutex<Vec<u8>>) -> Vec<u8> {
    std::mem::take(&mut *buf.lock().unwrap_or_else(PoisonError::into_inner))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn block<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(f);
        rt.shutdown_background();
        out
    }

    #[test]
    fn it_returns_merged_output_and_the_exit_code() {
        let r = block(run(
            "echo out; echo err >&2; exit 3",
            Path::new("."),
            USER_SHELL_TIMEOUT,
            CancellationToken::new(),
        ));
        assert!(
            r.output.contains("out") && r.output.contains("err"),
            "{}",
            r.output
        );
        assert_eq!(r.exit_code, Some(3));
    }

    #[test]
    fn it_runs_in_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker-file-0084"), "").unwrap();
        let r = block(run(
            "ls",
            dir.path(),
            USER_SHELL_TIMEOUT,
            CancellationToken::new(),
        ));
        assert!(r.output.contains("marker-file-0084"), "{}", r.output);
    }

    #[test]
    fn a_background_child_does_not_hold_it_open() {
        let (r, elapsed) = block(async {
            let t = Instant::now();
            let r = run(
                "sleep 30 & echo up",
                Path::new("."),
                Duration::from_secs(20),
                CancellationToken::new(),
            )
            .await;
            (r, t.elapsed())
        });
        assert!(r.output.contains("up") && elapsed < Duration::from_secs(10));
    }

    #[test]
    fn it_times_out_and_kills_the_group() {
        let (r, elapsed) = block(async {
            let t = Instant::now();
            let r = run(
                "sleep 5",
                Path::new("."),
                Duration::from_millis(300),
                CancellationToken::new(),
            )
            .await;
            (r, t.elapsed())
        });
        assert!(r.timed_out && !r.cancelled);
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    #[test]
    fn a_timeout_keeps_what_the_command_printed() {
        let r = block(run(
            "echo before; sleep 5",
            Path::new("."),
            Duration::from_millis(500),
            CancellationToken::new(),
        ));
        assert!(r.timed_out);
        assert!(r.output.contains("before"), "{:?}", r.output);
    }

    #[test]
    fn a_cancel_keeps_what_the_command_printed() {
        let r = block(async {
            let token = CancellationToken::new();
            let c = token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                c.cancel();
            });
            run(
                "echo before; sleep 5",
                Path::new("."),
                USER_SHELL_TIMEOUT,
                token,
            )
            .await
        });
        assert!(r.cancelled);
        assert!(r.output.contains("before"), "{:?}", r.output);
    }

    #[test]
    fn cancel_stops_it() {
        let (r, elapsed) = block(async {
            let t = Instant::now();
            let token = CancellationToken::new();
            let c = token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                c.cancel();
            });
            let r = run("sleep 5", Path::new("."), USER_SHELL_TIMEOUT, token).await;
            (r, t.elapsed())
        });
        assert!(r.cancelled && !r.timed_out);
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    #[test]
    fn output_is_capped() {
        let r = block(run(
            "yes | head -c 200000",
            Path::new("."),
            USER_SHELL_TIMEOUT,
            CancellationToken::new(),
        ));
        assert!(r.output.len() < 60_000, "{}", r.output.len());
        assert!(r.output.contains("[output truncated at 50KB"));
    }
}
