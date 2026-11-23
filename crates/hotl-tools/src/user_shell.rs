use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::builtins::{
    adopt, clip_output, collect_merged_into, kill_tree, merged_pipe, new_reaper, BASH_MAX_OUTPUT,
    BASH_OUTPUT_SLACK,
};

pub const USER_SHELL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellEnd {
    Exited(i32),
    #[cfg(unix)]
    Signaled(i32),
    TimedOut,
    Cancelled,
    Failed,
}

impl ShellEnd {
    pub fn label(self) -> Option<String> {
        match self {
            ShellEnd::Exited(0) => None,
            ShellEnd::Exited(n) => Some(format!("[exit {n}]")),
            #[cfg(unix)]
            ShellEnd::Signaled(sig) => {
                Some(format!("[killed by {}]", crate::builtins::signal_name(sig)))
            }
            ShellEnd::TimedOut => Some(format!(
                "[timed out after {}s]",
                USER_SHELL_TIMEOUT.as_secs()
            )),
            ShellEnd::Cancelled => Some("[cancelled]".to_string()),
            ShellEnd::Failed => Some("[could not run]".to_string()),
        }
    }

    pub fn shared_label(self) -> Option<String> {
        match self {
            ShellEnd::Exited(_) | ShellEnd::Cancelled | ShellEnd::Failed => None,
            _ => self.label(),
        }
    }

    pub fn exit_code(self) -> Option<i32> {
        match self {
            ShellEnd::Exited(n) => Some(n),
            _ => None,
        }
    }

    fn from_status(status: std::process::ExitStatus) -> Self {
        if let Some(code) = status.code() {
            return ShellEnd::Exited(code);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                return ShellEnd::Signaled(sig);
            }
        }
        ShellEnd::Failed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserShellRun {
    pub output: String,
    pub end: ShellEnd,
}

impl UserShellRun {
    fn failed(output: String) -> Self {
        Self {
            output,
            end: ShellEnd::Failed,
        }
    }
}

pub fn sanitize(raw: &str) -> String {
    let mut stripped = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            stripped.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                while let Some(&n) = chars.peek() {
                    match n {
                        '\u{20}'..='\u{3f}' => {
                            chars.next();
                        }
                        '\u{40}'..='\u{7e}' => {
                            chars.next();
                            break;
                        }
                        _ => break,
                    }
                }
            }
            Some(']') => {
                while let Some(n) = chars.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = String::with_capacity(stripped.len());
    for (i, line) in stripped.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        out.push_str(line.rsplit('\r').next().unwrap_or(line));
    }
    out
}

fn decode(bytes: &[u8]) -> String {
    clip_output(sanitize(&String::from_utf8_lossy(bytes)).as_bytes())
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
    let end = tokio::select! {
        result = &mut wait => return match result {
            Ok(status) => UserShellRun {
                output: decode(&take(&buf)),
                end: ShellEnd::from_status(status),
            },
            Err(e) => UserShellRun::failed(format!("Failed waiting on command: {e}.")),
        },
        _ = tokio::time::sleep(timeout) => ShellEnd::TimedOut,
        _ = cancel.cancelled() => ShellEnd::Cancelled,
    };
    kill_tree(&reaper);
    let _ = tokio::time::timeout(KILLED_DRAIN_GRACE, &mut wait).await;
    UserShellRun {
        output: decode(&take(&buf)),
        end,
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
        assert_eq!(r.end, ShellEnd::Exited(3));
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
        assert_eq!(r.end, ShellEnd::TimedOut);
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
        assert_eq!(r.end, ShellEnd::TimedOut);
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
        assert_eq!(r.end, ShellEnd::Cancelled);
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
        assert_eq!(r.end, ShellEnd::Cancelled);
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

    #[test]
    fn a_signal_death_is_named() {
        let r = block(run(
            "kill -TERM $$",
            Path::new("."),
            USER_SHELL_TIMEOUT,
            CancellationToken::new(),
        ));
        assert_eq!(r.end, ShellEnd::Signaled(15));
        assert_eq!(r.end.label().as_deref(), Some("[killed by SIGTERM]"));
    }

    #[test]
    fn a_missing_directory_is_failed() {
        let r = block(run(
            "echo hi",
            Path::new("/nonexistent-0085-dir"),
            USER_SHELL_TIMEOUT,
            CancellationToken::new(),
        ));
        assert_eq!(r.end, ShellEnd::Failed);
        assert!(!r.output.is_empty());
    }

    #[test]
    fn ansi_and_carriage_returns_are_cleaned() {
        let r = block(run(
            "printf '\\033[32mok\\033[0m\\nprog 1\\rprog 2\\n'",
            Path::new("."),
            USER_SHELL_TIMEOUT,
            CancellationToken::new(),
        ));
        assert_eq!(r.output, "ok\nprog 2\n");
    }

    #[test]
    fn non_utf8_output_is_lossy_not_a_panic() {
        let r = block(run(
            "printf '\\377\\376ok'",
            Path::new("."),
            USER_SHELL_TIMEOUT,
            CancellationToken::new(),
        ));
        assert!(r.output.ends_with("ok"), "{:?}", r.output);
        assert_eq!(r.end, ShellEnd::Exited(0));
    }

    #[test]
    fn sanitize_handles_each_sequence_shape() {
        assert_eq!(sanitize("a\x1b[1;31mb\x1b[0mc"), "abc");
        assert_eq!(sanitize("a\x1b]0;title\x07b"), "ab");
        assert_eq!(
            sanitize("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"),
            "alink"
        );
        assert_eq!(sanitize("x\r\ny\r\n"), "x\ny\n");
        assert_eq!(sanitize("a\rb\rc\nd"), "c\nd");
        assert_eq!(sanitize("ok\x1b[3"), "ok");
        assert_eq!(sanitize("ok\x1b"), "ok");
        assert_eq!(sanitize("ok\x1b]0;tit"), "ok");
        assert_eq!(sanitize("a\x1b[\nb"), "a\nb");
    }

    #[test]
    fn an_escape_cut_at_the_cap_leaves_no_fragment() {
        let r = block(run(
            "head -c 51190 /dev/zero | tr '\\0' a; printf '\\033[31mred\\033[0m'; head -c 5000 /dev/zero | tr '\\0' b",
            Path::new("."),
            USER_SHELL_TIMEOUT,
            CancellationToken::new(),
        ));
        assert!(!r.output.contains('\x1b'));
        assert!(!r.output.contains("[31m"));
        assert!(r.output.contains("[output truncated at 50KB"));
    }
}
