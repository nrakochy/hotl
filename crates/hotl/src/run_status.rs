use std::path::PathBuf;

use hotl_platform::{Ipc as _, KnownPaths as _};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Idle,
    Running,
    Waiting,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RunStatus {
    pub run_id: String,
    pub session_id: String,
    pub pid: u32,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub version: String,
    pub started_at_ms: u64,
    pub state: RunState,
}

pub fn run_dir() -> PathBuf {
    hotl_platform::KNOWN_PATHS
        .data()
        .unwrap_or_else(|| PathBuf::from(".").join("hotl"))
        .join("run")
}

pub fn status_path(run_id: &str) -> PathBuf {
    run_dir().join(format!("{run_id}.status.json"))
}

pub fn write(status: &RunStatus) -> std::io::Result<()> {
    use hotl_platform::PrivateFs as _;
    use std::io::Write as _;
    let path = status_path(&status.run_id);
    if let Some(dir) = path.parent() {
        hotl_platform::PRIVATE_FS.create_dir_all(dir)?;
    }
    let tmp = run_dir().join(format!("{}.status.json.tmp", status.run_id));
    let _ = std::fs::remove_file(&tmp);
    let body = serde_json::to_vec(status).map_err(std::io::Error::other)?;
    hotl_platform::PRIVATE_FS
        .create_file_new(&tmp, hotl_platform::Writes::FromStart)?
        .write_all(&body)?;
    std::fs::rename(&tmp, &path)
}

pub fn remove(run_id: &str) {
    let _ = std::fs::remove_file(status_path(run_id));
}

pub fn read_live() -> Vec<RunStatus> {
    let mut live: Vec<RunStatus> = hotl_platform::IPC
        .list_live()
        .into_iter()
        .filter_map(|id| {
            let text = std::fs::read_to_string(status_path(&id)).ok()?;
            serde_json::from_str(&text).ok()
        })
        .collect();
    live.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_id(tag: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        format!(
            "rs-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn status(run_id: &str, state: RunState) -> RunStatus {
        RunStatus {
            run_id: run_id.into(),
            session_id: "01JSTOREDSESSION".into(),
            pid: 4242,
            cwd: PathBuf::from("/work/here"),
            name: Some("named".into()),
            version: "9.9.9".into(),
            started_at_ms: 1_700_000_000_123,
            state,
        }
    }

    fn read_back(run_id: &str) -> RunStatus {
        serde_json::from_str(&std::fs::read_to_string(status_path(run_id)).unwrap()).unwrap()
    }

    #[test]
    fn write_then_read_round_trips() {
        let id = run_id("trip");
        write(&status(&id, RunState::Running)).unwrap();
        let back = read_back(&id);
        remove(&id);
        assert_eq!(back.run_id, id);
        assert_eq!(back.session_id, "01JSTOREDSESSION");
        assert_eq!(back.pid, 4242);
        assert_eq!(back.cwd, PathBuf::from("/work/here"));
        assert_eq!(back.name.as_deref(), Some("named"));
        assert_eq!(back.version, "9.9.9");
        assert_eq!(back.started_at_ms, 1_700_000_000_123);
        assert_eq!(back.state, RunState::Running);
        assert!(!status_path(&id).exists());
    }

    #[test]
    fn write_replaces_an_older_file_whole() {
        let id = run_id("replace");
        let mut first = status(&id, RunState::Waiting);
        first.name = Some("a much longer name than the second one carries".into());
        write(&first).unwrap();
        write(&status(&id, RunState::Idle)).unwrap();
        let back = read_back(&id);
        let tmp_left = run_dir().join(format!("{id}.status.json.tmp")).exists();
        remove(&id);
        assert_eq!(back, status(&id, RunState::Idle));
        assert!(!tmp_left);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let id = run_id("mode");
        write(&status(&id, RunState::Idle)).unwrap();
        let mode = std::fs::metadata(status_path(&id))
            .unwrap()
            .permissions()
            .mode();
        remove(&id);
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn json_keys_are_camel_case_and_state_is_lowercase() {
        let id = run_id("keys");
        write(&status(&id, RunState::Idle)).unwrap();
        let text = std::fs::read_to_string(status_path(&id)).unwrap();
        remove(&id);
        for key in [
            "\"runId\"",
            "\"sessionId\"",
            "\"startedAtMs\"",
            "\"state\":\"idle\"",
        ] {
            assert!(text.contains(key), "{key} missing from {text}");
        }
    }
}
