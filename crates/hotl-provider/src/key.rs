//! Key acquisition seam. Providers ask a `KeySource` for the current API key
//! instead of holding a static string, so short-lived credentials (gateway
//! virtual keys, vault-minted keys, OAuth tokens) can refresh mid-session.
//! Providers never know what mints the key.

use futures_util::future::BoxFuture;

/// Human-readable key-acquisition failure. Errors-are-prompts: the message
/// must say which command/source failed and what to fix.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyError(pub String);

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for KeyError {}

pub trait KeySource: Send + Sync {
    /// Current key. `None` = keyless (valid only for local OpenAI-compatible
    /// endpoints).
    fn get(&self) -> BoxFuture<'_, Result<Option<String>, KeyError>>;
    /// Re-acquire the key. No-op for static sources.
    fn refresh(&self) -> BoxFuture<'_, Result<(), KeyError>>;
    /// Whether `refresh()` can produce a different key.
    fn refreshable(&self) -> bool;
}

/// Today's behavior: a fixed key (or keyless), never refreshed.
pub struct StaticKey(pub Option<String>);

impl KeySource for StaticKey {
    fn get(&self) -> BoxFuture<'_, Result<Option<String>, KeyError>> {
        let v = self.0.clone();
        Box::pin(async move { Ok(v) })
    }
    fn refresh(&self) -> BoxFuture<'_, Result<(), KeyError>> {
        Box::pin(async { Ok(()) })
    }
    fn refreshable(&self) -> bool {
        false
    }
}

/// One-shot refresh gate for a provider send loop: on 401/403, refresh once
/// and retry once; any further auth failure on the same request surfaces.
/// Auth errors are still never blindly retried.
#[derive(Default)]
pub struct AuthRetry {
    refreshed: bool,
}

pub enum AuthAction {
    RefreshAndRetry,
    Surface,
}

impl AuthRetry {
    pub fn on_auth_error(&mut self, source_refreshable: bool) -> AuthAction {
        if source_refreshable && !self.refreshed {
            self.refreshed = true;
            AuthAction::RefreshAndRetry
        } else {
            AuthAction::Surface
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    #[test]
    fn static_key_returns_value_and_is_not_refreshable() {
        let s = StaticKey(Some("sk-abc".into()));
        assert_eq!(block_on(s.get()).unwrap(), Some("sk-abc".into()));
        assert!(!s.refreshable());
        block_on(s.refresh()).unwrap(); // no-op, no error
        assert_eq!(block_on(s.get()).unwrap(), Some("sk-abc".into()));
    }

    #[test]
    fn static_keyless_is_none() {
        assert_eq!(block_on(StaticKey(None).get()).unwrap(), None);
    }

    #[test]
    fn auth_retry_refreshes_once_then_surfaces() {
        let mut g = AuthRetry::default();
        assert!(matches!(g.on_auth_error(true), AuthAction::RefreshAndRetry));
        assert!(matches!(g.on_auth_error(true), AuthAction::Surface));
    }

    #[test]
    fn auth_retry_surfaces_immediately_for_static_sources() {
        let mut g = AuthRetry::default();
        assert!(matches!(g.on_auth_error(false), AuthAction::Surface));
    }
}
