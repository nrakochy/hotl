//! `HelperKey`: the api-key-helper `KeySource`. Runs a user-configured
//! command via `sh -c`; trimmed stdout is the key. Caches the key; a lapsed
//! TTL makes the next `get()` re-run the command (passive — no timers);
//! `refresh()` re-runs unconditionally (the 401/403 path).
//!
//! The command comes only from config.toml / env — editor-written planes —
//! and runs as harness infrastructure (never model-initiated), outside the
//! tool sandbox. Stderr goes to the error string shown to the human, never
//! into model context.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use hotl_provider::key::{KeyError, KeySource};

const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
const HELPER_STDOUT_CAP: usize = 64 * 1024;

pub struct HelperKey {
    command: String,
    ttl: Option<Duration>,
    timeout: Duration,
    cache: Mutex<Option<(String, Instant)>>,
}

impl HelperKey {
    pub fn new(command: String, ttl: Option<Duration>) -> Self {
        Self {
            command,
            ttl,
            timeout: HELPER_TIMEOUT,
            cache: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    async fn run_helper(&self) -> Result<String, KeyError> {
        let run = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(self.timeout, run)
            .await
            .map_err(|_| {
                KeyError(format!(
                    "api_key_helper `{}` timed out after {}s — it must print the key and exit quickly",
                    self.command,
                    self.timeout.as_secs()
                ))
            })?
            .map_err(|e| KeyError(format!("api_key_helper `{}` failed to start: {e}", self.command)))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let excerpt: String = stderr.trim().chars().take(200).collect();
            return Err(KeyError(format!(
                "api_key_helper `{}` exited with {}: {excerpt}",
                self.command, out.status
            )));
        }
        if out.stdout.len() > HELPER_STDOUT_CAP {
            return Err(KeyError(format!(
                "api_key_helper `{}` printed more than {HELPER_STDOUT_CAP} bytes — that is not a key",
                self.command
            )));
        }
        let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if key.is_empty() {
            return Err(KeyError(format!(
                "api_key_helper `{}` printed nothing on stdout — it must print the API key",
                self.command
            )));
        }
        Ok(key)
    }

    fn cached_fresh(&self) -> Option<String> {
        let cache = self.cache.lock().unwrap();
        let (key, at) = cache.as_ref()?;
        match self.ttl {
            Some(ttl) if at.elapsed() >= ttl => None,
            _ => Some(key.clone()),
        }
    }
}

impl KeySource for HelperKey {
    fn get(&self) -> BoxFuture<'_, Result<Option<String>, KeyError>> {
        Box::pin(async move {
            if let Some(key) = self.cached_fresh() {
                return Ok(Some(key));
            }
            let key = self.run_helper().await?;
            *self.cache.lock().unwrap() = Some((key.clone(), Instant::now()));
            Ok(Some(key))
        })
    }

    fn refresh(&self) -> BoxFuture<'_, Result<(), KeyError>> {
        Box::pin(async move {
            let key = self.run_helper().await?;
            *self.cache.lock().unwrap() = Some((key, Instant::now()));
            Ok(())
        })
    }

    fn refreshable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_provider::key::KeySource;

    /// Helper writing an invocation counter to a temp file, echoing key-N.
    fn counting_cmd(dir: &std::path::Path) -> String {
        let f = dir.join("count");
        format!(
            "c=$(cat {f} 2>/dev/null || echo 0); c=$((c+1)); echo $c > {f}; echo \"  key-$c \"",
            f = f.display()
        )
    }

    #[tokio::test]
    async fn stdout_is_trimmed_and_cached() {
        let dir = tempfile::tempdir().unwrap();
        let h = HelperKey::new(counting_cmd(dir.path()), None);
        assert_eq!(h.get().await.unwrap(), Some("key-1".into())); // trimmed
        assert_eq!(h.get().await.unwrap(), Some("key-1".into())); // cached, not re-run
    }

    #[tokio::test]
    async fn ttl_lapse_reruns_helper() {
        let dir = tempfile::tempdir().unwrap();
        let h = HelperKey::new(counting_cmd(dir.path()), Some(std::time::Duration::ZERO));
        assert_eq!(h.get().await.unwrap(), Some("key-1".into()));
        assert_eq!(h.get().await.unwrap(), Some("key-2".into())); // ttl 0 = always stale
    }

    #[tokio::test]
    async fn refresh_reruns_helper() {
        let dir = tempfile::tempdir().unwrap();
        let h = HelperKey::new(counting_cmd(dir.path()), None);
        assert_eq!(h.get().await.unwrap(), Some("key-1".into()));
        h.refresh().await.unwrap();
        assert_eq!(h.get().await.unwrap(), Some("key-2".into()));
        assert!(h.refreshable());
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_with_stderr_excerpt() {
        let h = HelperKey::new("echo broken-vault >&2; exit 3".into(), None);
        let e = h.get().await.unwrap_err();
        assert!(e.0.contains("exit"), "{}", e.0);
        assert!(e.0.contains("broken-vault"), "{}", e.0);
    }

    #[tokio::test]
    async fn empty_stdout_is_error() {
        let e = HelperKey::new("true".into(), None).get().await.unwrap_err();
        assert!(e.0.contains("printed nothing"), "{}", e.0);
    }

    #[tokio::test]
    async fn oversized_stdout_is_error() {
        let h = HelperKey::new("head -c 70000 /dev/zero | tr '\\0' 'a'".into(), None);
        let e = h.get().await.unwrap_err();
        assert!(e.0.contains("more than"), "{}", e.0);
    }

    #[tokio::test]
    async fn timeout_kills_helper() {
        let h = HelperKey::new("sleep 30".into(), None)
            .with_timeout(std::time::Duration::from_millis(100));
        let e = h.get().await.unwrap_err();
        assert!(e.0.contains("timed out"), "{}", e.0);
    }
}
