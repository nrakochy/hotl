//! Background jobs (0064): a `bash` call with `background: true` starts the
//! command under the same sandbox floor and the same permission decision as
//! a foreground call, returns at once with a file contract, and the session
//! is told when the job ends. No pipe, no drain thread: the child writes its
//! own `log`, so a chatty job never blocks and hotl holds only the `Child`,
//! the reaper and a stop token.
//!
//! Files, under `<session>.jobs/<job>/`: `cmd`, `pid`, `started` (unix ms),
//! `log` (stdout + stderr, write order), `rc` (exit code or `killed:SIG…`,
//! absent while running). Directory 0700, files 0600.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hotl_platform::PrivateFs as _;
use tokio_util::sync::CancellationToken;

/// How much of the log the completion notification carries (D4): a user
/// item is never evicted or cleared by the context ladder, so it must be
/// cheap on its own.
pub const JOB_TAIL_BYTES: usize = 8 * 1024;

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRc {
    Exit(i32),
    /// The signal (or Windows exception) name, e.g. `SIGKILL`.
    Killed(String),
}

impl std::fmt::Display for JobRc {
    /// The `rc` file's text: `0`, `137`, or `killed:SIGKILL`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exit(code) => write!(f, "{code}"),
            Self::Killed(sig) => write!(f, "killed:{sig}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    Done { rc: JobRc, ended_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// Six lowercase characters, unique within the session.
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub started_ms: u64,
    /// `detach: true` — outlives the session; `kill_all` skips it.
    pub detached: bool,
    pub state: JobState,
    /// `<session>.jobs/<id>`.
    pub dir: PathBuf,
    /// The `tool_use` id of the call that started it, for the surfaces.
    pub call_id: Option<String>,
}

impl JobRecord {
    pub fn log_path(&self) -> PathBuf {
        self.dir.join("log")
    }
    pub fn rc_path(&self) -> PathBuf {
        self.dir.join("rc")
    }
    /// The literal line that stops the whole tree, for the model to run
    /// through `bash` (D1).
    pub fn stop_line(&self) -> String {
        stop_line(self.pid)
    }
}

/// The platform's "kill this tree" command line.
pub fn stop_line(pid: u32) -> String {
    if cfg!(windows) {
        format!("taskkill /T /F /PID {pid}")
    } else {
        format!("kill -- -{pid}")
    }
}

/// What a `background` call asks for.
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub command: String,
    pub root: PathBuf,
    /// No default once backgrounded (D6); an explicit one is honoured with no cap.
    pub timeout: Option<Duration>,
    pub detach: bool,
}

/// What the registry tells the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    Started {
        id: String,
        call_id: Option<String>,
        pid: u32,
        command: String,
        detached: bool,
    },
    Done {
        id: String,
        call_id: Option<String>,
        command: String,
        rc: JobRc,
        secs: u64,
        /// The last `JOB_TAIL_BYTES` of the log, untrusted.
        tail: String,
        /// `timeout` or `/jobs` when hotl killed it.
        stopped_by: Option<&'static str>,
        log: PathBuf,
    },
}

pub type JobNotify = Arc<dyn Fn(JobEvent) + Send + Sync>;

struct Slot {
    record: JobRecord,
    stop: CancellationToken,
}

/// The per-session job registry.
#[derive(Default)]
pub struct Jobs {
    slots: Mutex<Vec<Slot>>,
}

impl Jobs {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Running jobs, detached or not.
    pub fn running(&self) -> usize {
        self.lock()
            .iter()
            .filter(|s| s.record.state == JobState::Running)
            .count()
    }

    /// Running jobs the session will kill when it ends.
    pub fn running_attached(&self) -> usize {
        self.lock()
            .iter()
            .filter(|s| s.record.state == JobState::Running && !s.record.detached)
            .count()
    }

    pub fn list(&self) -> Vec<JobRecord> {
        self.lock().iter().map(|s| s.record.clone()).collect()
    }

    /// Signal one job's waiter to kill the tree. `false` when no running job
    /// has that id. Reaches a detached job too: the human asked by name.
    pub fn stop(&self, id: &str) -> bool {
        let slots = self.lock();
        match slots
            .iter()
            .find(|s| s.record.id == id && s.record.state == JobState::Running)
        {
            Some(s) => {
                s.stop.cancel();
                true
            }
            None => false,
        }
    }

    /// Session end (D5): every running job that is not detached is killed.
    pub fn kill_all(&self) {
        for s in self.lock().iter() {
            if s.record.state == JobState::Running && !s.record.detached {
                s.stop.cancel();
            }
        }
    }

    fn mark_done(&self, id: &str, rc: JobRc, ended_ms: u64) {
        if let Some(s) = self.lock().iter_mut().find(|s| s.record.id == id) {
            s.record.state = JobState::Done { rc, ended_ms };
        }
    }
}

/// Everything a `bash` call needs to start a job, scoped per tool call by the
/// engine (the `PROGRESS_SINK` shape — `Registry::builtin()` is a
/// process-wide prototype, so a tool cannot hold session state).
pub struct JobContext {
    pub jobs: Arc<Jobs>,
    /// `<session>.jobs`.
    pub dir: PathBuf,
    /// `[behavior] max_jobs`: running jobs per session.
    pub max_jobs: usize,
    pub notify: JobNotify,
}

tokio::task_local! {
    pub static JOBS: Arc<JobContext>;
}

/// The job context of the call running on this task; `None` outside a
/// session (then `background` is a tool error).
pub fn job_context() -> Option<Arc<JobContext>> {
    JOBS.try_with(Clone::clone).ok()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Six lowercase chars off a fresh ULID; re-drawn on a collision.
fn fresh_id(taken: &[Slot]) -> String {
    loop {
        let ulid = hotl_types::new_ulid().to_ascii_lowercase();
        let id = ulid[ulid.len() - 6..].to_string();
        if !taken.iter().any(|s| s.record.id == id) {
            return id;
        }
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = hotl_platform::PRIVATE_FS.create_file_truncate(path)?;
    f.write_all(text.as_bytes())?;
    f.flush()
}

/// `rc` lands whole or not at all: temp beside it, then rename.
fn write_rc(dir: &Path, rc: &JobRc) -> std::io::Result<()> {
    let tmp = dir.join("rc.tmp");
    write_private(&tmp, &rc.to_string())?;
    std::fs::rename(tmp, dir.join("rc"))
}

/// The last `JOB_TAIL_BYTES` of the log, cut on a char boundary.
pub fn read_tail(log: &Path) -> String {
    let bytes = std::fs::read(log).unwrap_or_default();
    let start = bytes.len().saturating_sub(JOB_TAIL_BYTES);
    let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if start > 0 {
        // A cut mid-line reads better from the next line on.
        if let Some(nl) = text.find('\n') {
            text.drain(..=nl);
        }
    }
    text
}

#[cfg(unix)]
fn rc_of(status: std::process::ExitStatus) -> JobRc {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => JobRc::Exit(code),
        (None, Some(sig)) => JobRc::Killed(match sig {
            libc::SIGKILL => "SIGKILL".to_string(),
            libc::SIGTERM => "SIGTERM".to_string(),
            libc::SIGINT => "SIGINT".to_string(),
            libc::SIGHUP => "SIGHUP".to_string(),
            libc::SIGSEGV => "SIGSEGV".to_string(),
            libc::SIGABRT => "SIGABRT".to_string(),
            other => format!("signal {other}"),
        }),
        (None, None) => JobRc::Killed("unknown".into()),
    }
}

#[cfg(windows)]
fn rc_of(status: std::process::ExitStatus) -> JobRc {
    match status.code() {
        // The job object's kill reports STATUS_CONTROL_C_EXIT-class codes;
        // 1 is what `taskkill /F` leaves.
        Some(code) if code as u32 >= 0xC000_0000 => JobRc::Killed(format!("0x{code:08X}")),
        Some(code) => JobRc::Exit(code),
        None => JobRc::Killed("unknown".into()),
    }
}

impl JobContext {
    pub fn new(dir: PathBuf, max_jobs: usize, notify: JobNotify) -> Arc<Self> {
        Arc::new(Self {
            jobs: Arc::new(Jobs::default()),
            dir,
            max_jobs,
            notify,
        })
    }

    /// Start a job: files first, then the spawn, then the waiter. Every error
    /// is the message the model reads; nothing is left half-started.
    pub async fn start(&self, spec: JobSpec) -> Result<JobRecord, String> {
        let running: Vec<String> = self
            .jobs
            .list()
            .into_iter()
            .filter(|r| r.state == JobState::Running)
            .map(|r| format!("{} (pid {}: {})", r.id, r.pid, r.stop_line()))
            .collect();
        if running.len() >= self.max_jobs {
            return Err(format!(
                "{} background job(s) are already running — the cap is {} ([behavior] \
                 max_jobs). Stop one first: {}.",
                running.len(),
                self.max_jobs,
                running.join("; ")
            ));
        }
        let egress = crate::net::egress_state().await;
        let mut cmd = crate::builtins::prepare_command(&spec.command, &spec.root, &egress);
        let id = fresh_id(&self.jobs.lock());
        let dir = self.dir.join(&id);
        let io_err = |what: &str, e: std::io::Error| {
            format!(
                "Could not {what} for the background job at {}: {e}.",
                dir.display()
            )
        };
        hotl_platform::PRIVATE_FS
            .create_dir_all(&dir)
            .map_err(|e| io_err("create the job directory", e))?;
        let started_ms = now_ms();
        write_private(&dir.join("cmd"), &spec.command).map_err(|e| io_err("write `cmd`", e))?;
        write_private(&dir.join("started"), &started_ms.to_string())
            .map_err(|e| io_err("write `started`", e))?;
        let log = hotl_platform::PRIVATE_FS
            .create_file_truncate(&dir.join("log"))
            .map_err(|e| io_err("create `log`", e))?;
        let log_err = log.try_clone().map_err(|e| io_err("open `log` twice", e))?;
        // Detached on Windows means no job object at all (D5): a job object
        // would die with hotl's last handle. Unix keeps the reaper for `stop`
        // and simply never calls it from `kill_all`.
        let reaper = if spec.detach && cfg!(windows) {
            None
        } else {
            crate::builtins::new_reaper()
        };
        cmd.stdout(std::process::Stdio::from(log))
            .stderr(std::process::Stdio::from(log_err))
            .kill_on_drop(!spec.detach);
        let child = cmd.spawn().map_err(|e| {
            let _ = std::fs::remove_dir_all(&dir);
            format!("Could not start shell: {e}.")
        })?;
        crate::builtins::adopt(&reaper, &child);
        let pid = child.id().unwrap_or(0);
        write_private(&dir.join("pid"), &pid.to_string()).map_err(|e| io_err("write `pid`", e))?;
        let call_id = crate::current_call_id();
        let record = JobRecord {
            id: id.clone(),
            pid,
            command: spec.command.clone(),
            started_ms,
            detached: spec.detach,
            state: JobState::Running,
            dir: dir.clone(),
            call_id: call_id.clone(),
        };
        let stop = CancellationToken::new();
        self.jobs.lock().push(Slot {
            record: record.clone(),
            stop: stop.clone(),
        });
        (self.notify)(JobEvent::Started {
            id: id.clone(),
            call_id,
            pid,
            command: spec.command.clone(),
            detached: spec.detach,
        });
        tokio::spawn(waiter(
            child,
            reaper,
            stop,
            spec.timeout,
            record,
            Arc::clone(&self.jobs),
            Arc::clone(&self.notify),
        ));
        Ok(self
            .jobs
            .list()
            .into_iter()
            .find(|r| r.id == id)
            .expect("just pushed"))
    }
}

/// Owns the `Child` and the reaper until the job ends. The kill happens while
/// the child is un-reaped — the same pid-reuse invariant `bash_impl` keeps —
/// and `stop` only ever signals the token, never kills from outside.
async fn waiter(
    mut child: tokio::process::Child,
    reaper: crate::builtins::Reaper,
    stop: CancellationToken,
    timeout: Option<Duration>,
    record: JobRecord,
    jobs: Arc<Jobs>,
    notify: JobNotify,
) {
    let kill = |child: &mut tokio::process::Child| {
        if reaper.is_some() {
            crate::builtins::kill_tree(&reaper);
        } else {
            let _ = child.start_kill();
        }
    };
    let timeout_arm = async {
        match timeout {
            Some(t) => tokio::time::sleep(t).await,
            None => std::future::pending::<()>().await,
        }
    };
    let (status, stopped_by): (
        std::io::Result<std::process::ExitStatus>,
        Option<&'static str>,
    ) = tokio::select! {
        s = child.wait() => (s, None),
        _ = stop.cancelled() => {
            kill(&mut child);
            (child.wait().await, Some("/jobs"))
        }
        _ = timeout_arm => {
            kill(&mut child);
            (child.wait().await, Some("timeout"))
        }
    };
    let rc = match status {
        Ok(s) => rc_of(s),
        Err(_) => JobRc::Killed("unknown".into()),
    };
    let ended_ms = now_ms();
    let _ = write_rc(&record.dir, &rc);
    jobs.mark_done(&record.id, rc.clone(), ended_ms);
    let log = record.log_path();
    let tail = read_tail(&log);
    notify(JobEvent::Done {
        id: record.id,
        call_id: record.call_id,
        command: record.command,
        rc,
        secs: ended_ms.saturating_sub(record.started_ms) / 1000,
        tail,
        stopped_by,
        log,
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    type Seen = Arc<Mutex<Vec<JobEvent>>>;

    fn ctx(dir: &Path, max_jobs: usize) -> (Arc<JobContext>, Seen) {
        let seen: Seen = Arc::default();
        let s = Arc::clone(&seen);
        let notify: JobNotify = Arc::new(move |e| s.lock().unwrap().push(e));
        (JobContext::new(dir.join("s.jobs"), max_jobs, notify), seen)
    }

    fn spec(root: &Path, command: &str) -> JobSpec {
        JobSpec {
            command: command.into(),
            root: root.to_path_buf(),
            timeout: None,
            detach: false,
        }
    }

    async fn wait_done(seen: &Seen, id: &str) -> JobEvent {
        for _ in 0..200 {
            if let Some(e) = seen
                .lock()
                .unwrap()
                .iter()
                .find(|e| matches!(e, JobEvent::Done { id: d, .. } if d == id))
            {
                return e.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job {id} never finished: {:?}", seen.lock().unwrap());
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: kill(pid, 0) only probes.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[tokio::test]
    async fn a_started_job_writes_cmd_pid_started_and_no_rc() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, seen) = ctx(dir.path(), 8);
        let r = ctx.start(spec(dir.path(), "sleep 5")).await.unwrap();
        assert_eq!(r.id.len(), 6);
        assert!(r.pid > 0);
        assert_eq!(r.state, JobState::Running);
        let d = &r.dir;
        assert_eq!(std::fs::read_to_string(d.join("cmd")).unwrap(), "sleep 5");
        assert_eq!(
            std::fs::read_to_string(d.join("pid")).unwrap(),
            r.pid.to_string()
        );
        assert!(
            std::fs::read_to_string(d.join("started"))
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0
        );
        assert!(d.join("log").exists());
        assert!(!d.join("rc").exists(), "absent while running");
        assert_eq!(ctx.jobs.running(), 1);
        assert!(matches!(
            &seen.lock().unwrap()[0],
            JobEvent::Started { id, pid, .. } if *id == r.id && *pid == r.pid
        ));
        assert!(ctx.jobs.stop(&r.id));
        wait_done(&seen, &r.id).await;
    }

    #[tokio::test]
    async fn the_waiter_writes_rc_and_notifies_with_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, seen) = ctx(dir.path(), 8);
        let r = ctx
            .start(spec(dir.path(), "echo out; echo err >&2; exit 3"))
            .await
            .unwrap();
        let done = wait_done(&seen, &r.id).await;
        let JobEvent::Done {
            rc,
            tail,
            stopped_by,
            secs,
            log,
            ..
        } = done
        else {
            unreachable!()
        };
        assert_eq!(rc, JobRc::Exit(3));
        assert_eq!(tail, "out\nerr\n", "both streams, in write order");
        assert_eq!(stopped_by, None);
        assert!(secs < 5);
        assert_eq!(log, r.dir.join("log"));
        assert_eq!(std::fs::read_to_string(r.dir.join("rc")).unwrap(), "3");
        assert!(!r.dir.join("rc.tmp").exists());
        let listed = ctx.jobs.list();
        assert!(matches!(
            &listed[0].state,
            JobState::Done {
                rc: JobRc::Exit(3),
                ..
            }
        ));
        assert_eq!(ctx.jobs.running(), 0);
    }

    #[tokio::test]
    async fn the_tail_is_capped_at_job_tail_bytes_on_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        // 3-byte chars so a byte cut would land mid-char; the cap then also
        // drops the partial first line.
        let line = format!("{}\n", "é".repeat(100)); // 201 bytes
        let body: String = std::iter::repeat_n(line.as_str(), 60).collect(); // ~12 KiB
        std::fs::write(&log, &body).unwrap();
        let tail = read_tail(&log);
        assert!(tail.len() <= JOB_TAIL_BYTES, "{}", tail.len());
        assert!(tail.starts_with('é'), "starts on a whole line");
        assert!(tail.ends_with('\n'));
        assert!(
            tail.chars().all(|c| c == 'é' || c == '\n'),
            "no replacement char"
        );
        // Short logs come back whole.
        std::fs::write(&log, "a\nb\n").unwrap();
        assert_eq!(read_tail(&log), "a\nb\n");
    }

    #[tokio::test]
    async fn stop_kills_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, seen) = ctx(dir.path(), 8);
        let r = ctx
            .start(spec(dir.path(), "sleep 60 & sleep 60"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(alive(r.pid));
        assert!(ctx.jobs.stop(&r.id));
        let done = wait_done(&seen, &r.id).await;
        let JobEvent::Done { rc, stopped_by, .. } = done else {
            unreachable!()
        };
        assert_eq!(rc, JobRc::Killed("SIGKILL".into()));
        assert_eq!(stopped_by, Some("/jobs"));
        assert_eq!(
            std::fs::read_to_string(r.dir.join("rc")).unwrap(),
            "killed:SIGKILL"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        // The group is gone: a signal to -pgid finds nobody.
        // SAFETY: kill(-pgid, 0) only probes.
        assert_ne!(
            unsafe { libc::kill(-(r.pid as i32), 0) },
            0,
            "the group survived"
        );
        assert!(!ctx.jobs.stop(&r.id), "stopping a finished job is false");
        assert!(!ctx.jobs.stop("nope"));
    }

    #[tokio::test]
    async fn kill_all_skips_detached_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, seen) = ctx(dir.path(), 8);
        let attached = ctx.start(spec(dir.path(), "sleep 60")).await.unwrap();
        let detached = ctx
            .start(JobSpec {
                detach: true,
                ..spec(dir.path(), "sleep 60")
            })
            .await
            .unwrap();
        assert!(detached.detached);
        assert_eq!(ctx.jobs.running(), 2);
        assert_eq!(ctx.jobs.running_attached(), 1);
        ctx.jobs.kill_all();
        wait_done(&seen, &attached.id).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(alive(detached.pid), "a detached job outlives kill_all");
        assert_eq!(ctx.jobs.running(), 1);
        // The human can still stop it by name.
        assert!(ctx.jobs.stop(&detached.id));
        wait_done(&seen, &detached.id).await;
    }

    #[tokio::test]
    async fn a_timeout_kills_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, seen) = ctx(dir.path(), 8);
        let r = ctx
            .start(JobSpec {
                timeout: Some(Duration::from_millis(300)),
                ..spec(dir.path(), "sleep 60")
            })
            .await
            .unwrap();
        let JobEvent::Done { rc, stopped_by, .. } = wait_done(&seen, &r.id).await else {
            unreachable!()
        };
        assert_eq!(rc, JobRc::Killed("SIGKILL".into()));
        assert_eq!(stopped_by, Some("timeout"));
    }

    #[tokio::test]
    async fn the_registry_refuses_past_max_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, seen) = ctx(dir.path(), 2);
        let a = ctx.start(spec(dir.path(), "sleep 60")).await.unwrap();
        let b = ctx.start(spec(dir.path(), "sleep 60")).await.unwrap();
        let err = ctx.start(spec(dir.path(), "sleep 60")).await.unwrap_err();
        assert!(
            err.contains("2 background job(s) are already running"),
            "{err}"
        );
        assert!(err.contains(&a.id) && err.contains(&b.id), "{err}");
        assert!(err.contains(&format!("kill -- -{}", a.pid)), "{err}");
        assert_eq!(
            std::fs::read_dir(&ctx.dir).unwrap().count(),
            2,
            "no third dir"
        );
        ctx.jobs.kill_all();
        wait_done(&seen, &a.id).await;
        wait_done(&seen, &b.id).await;
        // A finished job frees its slot.
        let c = ctx.start(spec(dir.path(), "true")).await.unwrap();
        wait_done(&seen, &c.id).await;
    }

    #[tokio::test]
    async fn job_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (ctx, seen) = ctx(dir.path(), 8);
        let r = ctx.start(spec(dir.path(), "echo x")).await.unwrap();
        wait_done(&seen, &r.id).await;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&ctx.dir), 0o700);
        assert_eq!(mode(&r.dir), 0o700);
        for f in ["cmd", "pid", "started", "log", "rc"] {
            assert_eq!(mode(&r.dir.join(f)), 0o600, "{f}");
        }
    }

    #[test]
    fn background_and_foreground_share_one_command_builder() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let egress = crate::net::egress_state().await;
            let root = std::env::temp_dir();
            let fg = crate::builtins::prepare_command("echo hi", &root, &egress);
            let bg = crate::builtins::prepare_command("echo hi", &root, &egress);
            let (fg, bg) = (fg.as_std(), bg.as_std());
            assert_eq!(fg.get_program(), bg.get_program());
            assert_eq!(
                fg.get_args().collect::<Vec<_>>(),
                bg.get_args().collect::<Vec<_>>()
            );
            assert_eq!(fg.get_current_dir(), bg.get_current_dir());
            assert_eq!(fg.get_current_dir(), Some(root.as_path()));
        });
    }

    #[test]
    fn rc_renders_the_file_text() {
        assert_eq!(JobRc::Exit(0).to_string(), "0");
        assert_eq!(JobRc::Exit(137).to_string(), "137");
        assert_eq!(
            JobRc::Killed("SIGKILL".into()).to_string(),
            "killed:SIGKILL"
        );
        assert_eq!(stop_line(42), "kill -- -42");
    }
}
