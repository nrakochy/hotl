//! The single config file: `~/.config/hotl/config.toml`.
//!
//! One place for every hand-editable setting — provider, context/compaction,
//! behavior, retention, network egress — plus the domain sections (`[[allow]]`, `[[mcp]]`,
//! `[[hook]]`, `[diagnostics]`) that used to live in their own files. The
//! prose/data that isn't really "settings" stays separate: `system-prompt.md`,
//! `memory/`, `skills/`, and the machine-written `trust.toml`.
//!
//! Precedence for scalar settings: **environment variable > config.toml >
//! built-in default**, so existing `HOTL_*`-based setups and CI keep working.
//! For the domain sections, config.toml wins if it defines them; otherwise the
//! legacy standalone file (`permissions.toml`, `mcp.toml`, `hooks.toml`) is
//! read as a fallback so no one's setup breaks.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderCfg,
    #[serde(default)]
    pub context: ContextCfg,
    #[serde(default)]
    pub behavior: BehaviorCfg,
    #[serde(default)]
    pub retention: RetentionCfg,
    #[serde(default)]
    pub history: HistoryCfg,
    #[serde(default)]
    pub network: NetworkCfg,
    #[serde(default)]
    pub permissions: PermissionsCfg,
    #[serde(default)]
    pub skills: SkillsCfg,
    #[serde(default)]
    pub agents: AgentsCfg,
    #[serde(default)]
    pub concurrency: ConcurrencyCfg,
    #[serde(default)]
    pub sandbox: SandboxCfg,
    /// Raw document, for reserializing the domain sections to their loaders.
    #[serde(skip)]
    raw: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SkillsCfg {
    /// `false` stops reading Claude Code skill roots (`~/.claude/skills`,
    /// the plugin cache). Default: read them when present.
    pub claude: Option<bool>,
    /// `[skills.marketplaces]` — extra skill sources: name → local path
    /// (read in place) or git URL (managed checkout; `hotl skills add`).
    #[serde(default)]
    pub marketplaces: std::collections::BTreeMap<String, String>,
}

impl SkillsCfg {
    /// Resolve `[skills.marketplaces]` to discovery roots: a local path is
    /// read in place (`~` expanded), a git URL maps to its managed checkout
    /// under `<config_dir>/marketplaces/<name>`. An invalid name is skipped
    /// with a warning — fail-closed, a bad entry never loads skills.
    pub fn marketplace_roots(
        &self,
        config_dir: &Path,
    ) -> (Vec<(String, std::path::PathBuf)>, Vec<String>) {
        let mut roots = Vec::new();
        let mut warnings = Vec::new();
        for (name, source) in &self.marketplaces {
            let Some(name) = hotl_tools::skills::normalize_marketplace_name(name) else {
                warnings.push(format!(
                    "[skills.marketplaces] `{name}` is not a valid marketplace name \
                     (letters, digits, `.`/`_`/`-`, alphanumeric first char, ≤ 64 chars) \
                     — entry skipped"
                ));
                continue;
            };
            let dir = if is_git_url(source) {
                config_dir.join("marketplaces").join(&name)
            } else {
                expand_home(source)
            };
            roots.push((name, dir));
        }
        (roots, warnings)
    }
}

/// `[agents]` — user-defined subagent shapes (tier-1 gap #6). Mirrors
/// `[skills] claude` exactly: hotl always reads its own `agents/*.md`;
/// `~/.claude/agents/*.md` loads too unless opted out.
#[derive(Debug, Default, Deserialize)]
pub struct AgentsCfg {
    /// `false` stops reading `~/.claude/agents`. Default: read it when
    /// present.
    pub claude: Option<bool>,
}

/// A git URL as opposed to a local path: a fetch scheme, an scp-style
/// `git@` prefix, or a trailing `.git`.
pub fn is_git_url(source: &str) -> bool {
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || source.ends_with(".git")
}

/// `$HOME`, or `None` where the environment does not name one (a daemon with
/// a scrubbed env). The credential read-carve is `$HOME`-relative, so no home
/// simply means no Tier B — narrower, never wider.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Expand a leading `~/` against `$HOME`.
fn expand_home(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

#[derive(Debug, Default, Deserialize)]
pub struct PermissionsCfg {
    /// `"bypass"` (default — no ordinary prompts) | `"ask"` | `"dontask"`.
    /// `"auto"` is the old name for `"bypass"` and still parses.
    pub mode: Option<String>,
    /// Plan mode, the axis orthogonal to `mode`: file edits always ask.
    pub plan: Option<bool>,
}

/// The resolved `[permissions]` state: both axes plus one warning to print.
pub struct ResolvedPermissions {
    pub mode: hotl_tools::rules::PermissionMode,
    pub plan: bool,
    pub warning: Option<String>,
}

impl PermissionsCfg {
    /// Resolve both axes: env (`HOTL_PERMISSIONS` / `HOTL_PLAN`) > config >
    /// default (`bypass`, plan off). An unknown mode fails **closed** to `Ask`
    /// with a warning — a typo must never silently mean "don't prompt".
    ///
    /// A `mode` of `"plan"` predates the overlay split: it turns plan on and
    /// leaves the mode at the default, rather than failing closed as a typo
    /// would (see [`hotl_tools::rules::is_legacy_plan_word`]).
    pub fn resolve(&self, env: Option<&str>, plan_env: Option<&str>) -> ResolvedPermissions {
        use hotl_tools::rules::{is_legacy_plan_word, PermissionMode};
        let source = env.or(self.mode.as_deref());
        let (mode, plan_from_mode, warning) = match source {
            None => (PermissionMode::Bypass, false, None),
            Some(s) if is_legacy_plan_word(s) => (PermissionMode::Bypass, true, None),
            Some(s) => match PermissionMode::from_str(s) {
                Some(m) => (m, false, None),
                None => (
                    PermissionMode::Ask,
                    false,
                    Some(format!(
                        "[permissions].mode = \"{s}\" is not a mode (ask | bypass | dontask) — failing closed to \"ask\""
                    )),
                ),
            },
        };
        // Anything but an explicit falsey word turns it on: `HOTL_PLAN=1`,
        // `=true`, and a bare `HOTL_PLAN=` all read as "the user asked for it".
        let plan = match plan_env {
            Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false"),
            None => self.plan.unwrap_or(plan_from_mode),
        };
        ResolvedPermissions {
            mode,
            plan,
            warning,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ProviderCfg {
    /// `provider/model`, e.g. `openai/gpt-5` or `anthropic/claude-opus-4-8`.
    pub model: Option<String>,
    /// Endpoint for the active provider. `openai/…` has always used this;
    /// `anthropic/…` honors it too, so any Anthropic-shaped endpoint (a local
    /// bridge, a gateway) is reachable. Only one provider is active at a time,
    /// so one key serves both.
    pub base_url: Option<String>,
    /// `"api_key"` (default) | `"subscription"`. Under `subscription` hotl
    /// holds no credential — the endpoint authenticates upstream on its own —
    /// and `base_url` is required. Provider-neutral: it reads the same for
    /// `anthropic/…` and `openai/…`.
    pub auth: Option<String>,
    /// Cheap model for compaction summaries.
    pub fast_model: Option<String>,
    /// Command whose stdout (trimmed) is the API key. When set, it beats the
    /// static key env vars: configuring a helper is a deliberate act.
    pub api_key_helper: Option<String>,
    /// Re-run the helper when the cached key is older than this. Absent =
    /// refresh only at startup and on auth failure.
    pub api_key_helper_ttl_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ContextCfg {
    pub window: Option<u64>,
    pub compaction_reset: Option<bool>,
    pub show_used_pct: Option<bool>,
    pub evict_tokens: Option<u64>,
}

// Dead until `agent.rs::engine_config` calls these — decision-log request #1
// of specs/exec-plans/active/0014-remediation-model-registry.md, owned by R10.
// `hotl` is a bin-only crate, so `pub` confers no reachability and the lint
// fires despite the tests below exercising both. Remove this attribute when
// that call site lands; the lint then holds the seam honest again.
#[allow(dead_code)]
impl ContextCfg {
    /// Resolve the compaction context window, in tokens.
    ///
    /// Precedence, highest first: `HOTL_CONTEXT_WINDOW` (passed as `env`) >
    /// `[context] window` > the model's catalogued window >
    /// [`hotl_provider::catalog::FALLBACK_CONTEXT_WINDOW`]. The explicit tiers
    /// still win: a gateway that trims the window is a real configuration and
    /// the catalog must never overrule the person who said so.
    ///
    /// The second element is a startup warning, present only when an
    /// uncatalogued model fell through to the fallback. Silence there is the
    /// original defect: a 1M-window model compacting at 160K, or an 8K local
    /// model overflowing on turn two, with nothing on screen either way.
    ///
    /// INVARIANT: an explicit `[context] window` or `HOTL_CONTEXT_WINDOW` is
    /// always honored verbatim. Enforced by
    /// `explicit_config_and_env_still_beat_the_catalog`.
    pub fn resolve_window(&self, model: &str, env: Option<&str>) -> (u64, Option<String>) {
        if let Some(window) = env.and_then(|v| v.parse::<u64>().ok()).or(self.window) {
            return (window, None);
        }
        match hotl_provider::catalog::context_window(model) {
            Some(window) => (window, None),
            None => (
                hotl_provider::catalog::FALLBACK_CONTEXT_WINDOW,
                Some(format!(
                    "hotl: `{model}` is not in the model catalog — assuming a \
                     {} token context window. If that is wrong, set \
                     `[context] window` in config.toml (or HOTL_CONTEXT_WINDOW); \
                     too high overflows the model, too low compacts early.",
                    hotl_provider::catalog::FALLBACK_CONTEXT_WINDOW
                )),
            ),
        }
    }

    /// The token-estimation profile for `model`. An uncatalogued model gets
    /// the conservative default rather than a guessed ratio — overcounting is
    /// the only safe direction (see `hotl_context::tokens`).
    pub fn token_profile(&self, model: &str) -> hotl_context::TokenProfile {
        match hotl_provider::catalog::lookup(model) {
            Some(info) => {
                hotl_context::TokenProfile::from_chars_per_token(info.ascii_chars_per_token)
            }
            None => hotl_context::TokenProfile::CONSERVATIVE,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct BehaviorCfg {
    /// `false` disables the bash sandbox floor.
    pub sandbox: Option<bool>,
    /// Vim-style keys in the console's input editor. Resolve via
    /// [`BehaviorCfg::vim_mode`] — absent means **off**.
    pub vim_mode: Option<bool>,
    /// Terminal mouse capture: the wheel scrolls the transcript and dragging
    /// selects. Resolve via [`BehaviorCfg::mouse`] — absent means **on**.
    /// `HOTL_MOUSE=0` overrides this, per the env-beats-config precedence.
    pub mouse: Option<bool>,
    /// Copy a mouse drag-selection to the clipboard on release. Resolve via
    /// [`BehaviorCfg::copy_on_select`] — absent means **on**. Requires
    /// `mouse`; without capture there are no drag events to act on.
    pub copy_on_select: Option<bool>,
    /// Model samples one prompt may spend before the turn is cut short with
    /// `turn limit reached`. A tool round-trip costs one, so this is the
    /// agent loop's step budget. `-1` = unlimited (run until the model stops
    /// on its own). Absent = the engine default.
    pub max_turns: Option<i64>,
}

impl BehaviorCfg {
    /// Default **off**: a modal editor is a trap for anyone who doesn't
    /// already have the muscle memory — one stray `Esc` and typing stops
    /// inserting. Vim users opt in with `[behavior] vim_mode = true`.
    ///
    /// This is deliberately the opposite of `hotl watch`'s `[settings]
    /// vim_mode`, which stays on: there the letter keys are *additive* over a
    /// read-only list (arrows, `enter`, `q`, and `r` work either way), so
    /// leaving them on strands nobody.
    pub fn vim_mode(&self) -> bool {
        self.vim_mode.unwrap_or(false)
    }

    /// Default **on**: the wheel scrolling the transcript is what most people
    /// reach for first. The cost is the terminal's own drag-select, which
    /// `copy_on_select` gives back from inside the app — and `Shift` still
    /// bypasses capture on most emulators for anyone who wants the real thing.
    pub fn mouse(&self) -> bool {
        self.mouse.unwrap_or(true)
    }

    /// Default **on**: dragging out a region and finding it on the clipboard
    /// is what capture took away, so restoring it is the unsurprising posture.
    /// Turning it off returns the console to wheel-scroll only.
    pub fn copy_on_select(&self) -> bool {
        self.copy_on_select.unwrap_or(true)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RetentionCfg {
    pub max_age_days: Option<u64>,
    pub max_sessions: Option<usize>,
}

/// `[history]` — the interactive console's prompt history: a size-bounded,
/// on-disk recall file. Every field is optional; the getters below carry the
/// product defaults (on, 1000 entries, 2 MiB).
#[derive(Debug, Default, Deserialize)]
pub struct HistoryCfg {
    pub enabled: Option<bool>,
    pub path: Option<String>,
    pub max_entries: Option<usize>,
    pub max_bytes: Option<u64>,
}

impl HistoryCfg {
    /// Default on — recall is the expected console behavior.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries.unwrap_or(1000)
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.unwrap_or(2 * 1024 * 1024)
    }

    /// The resolved history file: a user `path` (with `~/` expanded), else
    /// `<data_dir>/history.jsonl` (history is state/data, not config).
    pub fn resolved_path(&self, data_dir: &Path) -> std::path::PathBuf {
        match &self.path {
            Some(p) => expand_home(p),
            None => data_dir.join("history.jsonl"),
        }
    }
}

/// `[concurrency]` — the Layer-B resource governors (`hotl_tools::concurrency`
/// index spec) plus the two Layer-C runtime-pool knobs. `agents`/`requests`/
/// `subprocs` feed `ConcurrencyLimits` (env `HOTL_CONCURRENCY_*` > this >
/// the fixed default; `0`/absent falls back to the default, never to zero).
///
/// Of the two Layer-C knobs, only `blocking_threads` is actually wired:
/// `main.rs::block_on` calls `.max_blocking_threads()` on hotl's existing
/// `current_thread` runtime builder — a call valid on any runtime flavor —
/// sizing the pool `glob`'s `spawn_blocking` tree walk (the sole real
/// blocking-pool user) draws from. `worker_threads` stays deliberately
/// inert: it only applies to a `multi_thread` runtime, and hotl runs a
/// single `current_thread` runtime everywhere (`main.rs::block_on` drives
/// every subcommand including the TUI and `acp_main`; `doctor.rs` builds its
/// own `current_thread` runtime too) by design — switching flavors to honor
/// it risks breaking `!Send` futures across the TUI/actor code, which is out
/// of scope here. `agent::layer_c_warning` surfaces that inertness at
/// startup so a configured `worker_threads` is visible, not a silent no-op.
#[derive(Debug, Default, Deserialize)]
pub struct ConcurrencyCfg {
    /// Concurrent sub-agent LLM sessions — governs `spawn`'s fan-out.
    pub agents: Option<usize>,
    /// Concurrent `web_fetch`/`web_search` HTTP requests — governs
    /// `web_fetch`'s multi-URL batch today.
    pub requests: Option<usize>,
    /// Concurrent `bash`/`grep`/hook child processes (unused until a
    /// subprocess-fan-out plan lands).
    pub subprocs: Option<usize>,
    /// Reserved: tokio worker-thread pool size (`0` = num_cpus). Parsed, but
    /// deliberately inert — see the struct doc.
    pub worker_threads: Option<usize>,
    /// `spawn_blocking` pool cap (index default 16; tokio's own default is
    /// 512 if unset). Wired: `main.rs::block_on` applies it to the
    /// `current_thread` runtime's blocking pool.
    pub blocking_threads: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct NetworkCfg {
    /// `"open"` (default) | `"off"` | `"allowlist"`.
    pub egress: Option<String>,
    /// Hosts reachable in allowlist mode (`"github.com"`, `"*.crates.io"`).
    #[serde(default)]
    pub allow: Vec<String>,
}

impl NetworkCfg {
    /// Resolve `[network]` to the process egress policy. An unknown mode
    /// fails **closed** to `Off` with a loud warning — a typo must never
    /// mean "open".
    pub fn egress_policy(&self) -> (hotl_tools::net::EgressPolicy, Option<String>) {
        use hotl_tools::net::EgressPolicy;
        match self.egress.as_deref() {
            None | Some("open") => (EgressPolicy::Open, None),
            Some("off") => (EgressPolicy::Off, None),
            Some("allowlist") => (EgressPolicy::Allowlist(self.allow.clone()), None),
            Some(other) => (
                EgressPolicy::Off,
                Some(format!(
                    "config.toml [network].egress = \"{other}\" is not a mode \
                     (open | off | allowlist) — failing closed to \"off\""
                )),
            ),
        }
    }
}

/// `[sandbox]` — owner-configured widening of the kernel write floor.
#[derive(Debug, Default, Deserialize)]
pub struct SandboxCfg {
    /// Extra directories sandboxed commands may write to, on top of the
    /// working directory, temp, and `/dev`. `~/` expands; missing dirs are
    /// created at startup; hotl's own config/data dirs can never be listed.
    #[serde(default)]
    pub writable: Vec<String>,
    /// Paths lifted out of the default credential **read**-deny for sandboxed
    /// commands. Mirrors `writable` — absolute, `~/` expands, canonicalized,
    /// and refused if it touches hotl's own config or data dir — with two
    /// deliberate differences: a missing directory is never created (creating
    /// `~/.ssh` as a side effect of parsing config would be surprising and
    /// wrong), and a missing entry is dropped with a warning rather than an
    /// error, because a read-deny for a path that does not exist is a no-op.
    #[serde(default)]
    pub readable: Vec<String>,
    /// `"workspace"` (default) | `"writable"` — how far the mutating file
    /// tools (`write`, `edit`) follow the `writable` roots.
    pub file_tools: Option<String>,
}

impl SandboxCfg {
    /// Resolve `[sandbox]` to the extras `hotl_tools::sandbox::init_extras`
    /// installs. Fail-closed, entry by entry: a bad entry is dropped with a
    /// warning and never takes the rest down.
    ///
    /// Refusal wins over every other rule: an entry that is, contains, or is
    /// inside the config or data dir would let a sandboxed command rewrite
    /// hotl's own allow-rules/hooks (config) or tamper with session state
    /// (data) — self-granted privilege escalation. That refusal is what makes
    /// `~` and `/` impossible to list. All comparisons run on canonicalized
    /// forms so a symlink cannot smuggle a protected dir in; the one residue
    /// (documented wart) is that a symlinked *ancestor* can leave a freshly
    /// created empty dir behind before the canonical check refuses the entry.
    pub fn resolve(
        &self,
        config_dir: &Path,
        data_dir: &Path,
    ) -> (hotl_tools::sandbox::SandboxExtras, Vec<String>) {
        use hotl_tools::sandbox::FileToolsMode;
        let mut warnings = Vec::new();
        let mut writable: Vec<std::path::PathBuf> = Vec::new();
        let protected = [canon_or(config_dir), canon_or(data_dir)];
        let refusal = |entry: &str| {
            format!(
                "[sandbox].writable `{entry}` would expose hotl's own config/state \
                 (allow-rules, hooks, session logs) to sandboxed commands — entry refused"
            )
        };
        for entry in &self.writable {
            let expanded = expand_home(entry);
            if !expanded.is_absolute() {
                warnings.push(format!(
                    "[sandbox].writable `{entry}` is not an absolute path — entry ignored"
                ));
                continue;
            }
            // Lexical pre-check before any mkdir side effect.
            if protected.iter().any(|p| overlaps(&expanded, p))
                || overlaps(&expanded, config_dir)
                || overlaps(&expanded, data_dir)
            {
                warnings.push(refusal(entry));
                continue;
            }
            if !expanded.exists() {
                if let Err(e) = std::fs::create_dir_all(&expanded) {
                    warnings.push(format!(
                        "[sandbox].writable `{entry}` cannot be created: {e} — entry ignored"
                    ));
                    continue;
                }
            }
            let resolved = match expanded.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    warnings.push(format!(
                        "[sandbox].writable `{entry}` cannot be resolved: {e} — entry ignored"
                    ));
                    continue;
                }
            };
            if !resolved.is_dir() {
                warnings.push(format!(
                    "[sandbox].writable `{entry}` is not a directory — entry ignored"
                ));
                continue;
            }
            // The canonical refusal check — symlinks resolved on both sides.
            if protected.iter().any(|p| overlaps(&resolved, p)) {
                warnings.push(refusal(entry));
                continue;
            }
            if let Some(root) = risky_root(&resolved) {
                warnings.push(format!(
                    "[sandbox].writable `{entry}` grants writes under the system root \
                     {root} — honored, but binaries and configuration living there \
                     become writable to every sandboxed command"
                ));
            }
            if !writable.contains(&resolved) {
                writable.push(resolved);
            }
        }
        let file_tools = match self.file_tools.as_deref() {
            None | Some("workspace") => FileToolsMode::Workspace,
            Some("writable") => FileToolsMode::Writable,
            Some(other) => {
                warnings.push(format!(
                    "config.toml [sandbox].file_tools = \"{other}\" is not a mode \
                     (workspace | writable) — failing closed to \"workspace\""
                ));
                FileToolsMode::Workspace
            }
        };
        if file_tools == FileToolsMode::Writable && writable.is_empty() {
            warnings.push(
                "[sandbox].file_tools = \"writable\" has no effect: `writable` is empty \
                 (or every entry was refused)"
                    .to_string(),
            );
        }
        let read_deny = self.resolve_read_deny(
            config_dir,
            data_dir,
            &hotl_tools::sandbox::write_roots_with(&writable),
            home_dir().as_deref(),
            &mut warnings,
        );
        (
            hotl_tools::sandbox::SandboxExtras {
                writable,
                file_tools,
                read_deny,
            },
            warnings,
        )
    }

    /// The two read-carve tiers (plan 0022).
    ///
    /// Tier A is hotl's own config and data dirs — the session token under the
    /// data dir drives the session socket, which *is* the permission gate, so
    /// a confined child that can read it has escalated out of the gate
    /// entirely. It is built here rather than accepted from config, and the
    /// `readable` refusal below is what keeps it that way. The run dir lives
    /// inside the data dir (`session_server::run_dir` = `data_dir()/run`), so
    /// denying the data dir already covers it — that is why it has no entry.
    ///
    /// Tier B is the credential class, minus every `readable` entry.
    ///
    /// `granted` (the resolved kernel write set) and `home` are parameters
    /// rather than reads of process state so the unit tests can drive them —
    /// the same `_with`-seam convention `sandbox.rs` uses. Without it a test
    /// running from a tempdir would see its own `$HOME` swallowed by the
    /// `TMPDIR` write root.
    fn resolve_read_deny(
        &self,
        config_dir: &Path,
        data_dir: &Path,
        granted: &[std::path::PathBuf],
        home: Option<&Path>,
        warnings: &mut Vec<String>,
    ) -> hotl_tools::sandbox::ReadDeny {
        let mut readable: Vec<std::path::PathBuf> = Vec::new();
        for entry in &self.readable {
            let expanded = expand_home(entry);
            if !expanded.is_absolute() {
                warnings.push(format!(
                    "[sandbox].readable `{entry}` is not an absolute path — entry ignored"
                ));
                continue;
            }
            // Lifting hotl's own dirs is Tier A self-granted escalation — the
            // same refusal `writable` applies, for the same reason.
            if overlaps(&expanded, config_dir) || overlaps(&expanded, data_dir) {
                warnings.push(readable_refusal(entry));
                continue;
            }
            if !expanded.exists() {
                warnings.push(format!(
                    "[sandbox].readable `{entry}` does not exist — entry ignored (nothing \
                     there is denied, so lifting it is a no-op)"
                ));
                continue;
            }
            let resolved = match expanded.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    warnings.push(format!(
                        "[sandbox].readable `{entry}` cannot be resolved: {e} — entry ignored"
                    ));
                    continue;
                }
            };
            // The canonical refusal — symlinks resolved on both sides.
            if overlaps(&resolved, &canon_or(config_dir))
                || overlaps(&resolved, &canon_or(data_dir))
            {
                warnings.push(readable_refusal(entry));
                continue;
            }
            if let Some(root) = risky_root(&resolved) {
                warnings.push(format!(
                    "[sandbox].readable `{entry}` lifts the read-deny under the system root \
                     {root} — honored, but nothing there is carved out for sandboxed commands"
                ));
            }
            if !readable.contains(&resolved) {
                readable.push(resolved);
            }
        }
        let always = vec![canon_or(config_dir), canon_or(data_dir)];
        // A deny entry that sits inside a write grant cannot be honored:
        // Landlock resolves against the closest matching rule, so the
        // `from_all` grant on the ancestor re-opens the read. Drop it loudly
        // rather than shipping a denial the kernel will not deliver.
        let mut secrets = Vec::new();
        for path in home
            .map(hotl_tools::sandbox::credential_read_paths)
            .unwrap_or_default()
        {
            let resolved = canon_or(&path);
            if readable.iter().any(|r| overlaps(&resolved, r)) {
                continue; // explicitly lifted by the operator
            }
            if let Some(root) = granted.iter().find(|g| resolved.starts_with(g)) {
                warnings.push(format!(
                    "[sandbox] cannot deny reads of {} — it sits inside the writable root {} \
                     and the kernel resolves the closest rule, which grants read there",
                    resolved.display(),
                    root.display()
                ));
                continue;
            }
            secrets.push(resolved);
        }
        hotl_tools::sandbox::ReadDeny { always, secrets }
    }
}

fn readable_refusal(entry: &str) -> String {
    format!(
        "[sandbox].readable `{entry}` would expose hotl's own config/state (allow-rules, \
         hooks, the session token) to sandboxed commands — entry refused"
    )
}

/// Canonical when possible, lexical when the path does not exist (a fresh
/// config dir before `hotl setup`).
fn canon_or(p: &Path) -> std::path::PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Equal, ancestor, or descendant — any of the three lets writes cross into
/// the other's subtree.
fn overlaps(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

/// System roots that are honored with a loud warning: making them writable
/// hands every sandboxed command the binaries/configuration living there.
/// `/var` is deliberately absent — macOS keeps the user temp tree under
/// `/var/folders` (already inside the floor), so warning on it would tag
/// every temp-adjacent path while carrying little signal of its own.
fn risky_root(p: &Path) -> Option<&'static str> {
    const RISKY: &[&str] = &[
        "/etc",
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/boot",
        "/opt",
        "/System",
        "/Library",
        "/Applications",
    ];
    RISKY
        .iter()
        .find(|root| overlaps(p, &canon_or(Path::new(root))))
        .copied()
}

impl Config {
    /// Load `config.toml`; a malformed file warns and yields defaults
    /// (fail-closed: a typo never silently changes a setting).
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join("config.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match text.parse::<toml::Value>() {
            Ok(raw) => {
                // Parse the typed settings from the same source string (no
                // deep clone of the raw document just to deserialize it).
                let mut cfg: Config = toml::from_str(&text).unwrap_or_default();
                cfg.raw = Some(raw);
                cfg
            }
            Err(e) => {
                eprintln!("hotl: config.toml ignored (parse error): {e}");
                Self::default()
            }
        }
    }

    /// The `[[allow]]` rules as a standalone TOML string (for `Rules::from_toml`),
    /// or `None` if config.toml has no `allow` section (→ fall back to the file).
    pub fn allow_toml(&self) -> Option<String> {
        self.section_as_toml("allow")
    }

    /// The `[[mcp]]` servers as a `[[server]]`-shaped TOML string (matching the
    /// legacy `mcp.toml` schema), or `None`.
    pub fn mcp_toml(&self) -> Option<String> {
        let servers = self.raw.as_ref()?.get("mcp")?;
        toml::to_string(&toml::toml! { server = (servers.clone()) }).ok()
    }

    /// The configured MCP servers, parsed. The registry and `hotl mcp` both
    /// come through here so a roster can never disagree with what a turn loads.
    /// A `[[mcp]]` section that fails to parse yields none, matching the
    /// registry's prior behaviour of skipping the tool entirely.
    pub fn mcp_servers(&self) -> Vec<hotl_mcp::config::ServerConfig> {
        self.mcp_toml()
            .and_then(|t| toml::from_str::<hotl_mcp::config::McpConfig>(&t).ok())
            .map(|c| c.servers)
            .unwrap_or_default()
    }

    /// The `[[retrieval]]` backends as a `[[backend]]`-shaped TOML string
    /// (for `hotl_retrieval::config::RetrievalConfig`), or `None`.
    pub fn retrieval_toml(&self) -> Option<String> {
        let backends = self.raw.as_ref()?.get("retrieval")?;
        toml::to_string(&toml::toml! { backend = (backends.clone()) }).ok()
    }

    /// The `[web]` section's *contents* (for `hotl_tools::web::WebConfig`,
    /// whose `search` field sits at the document's top level — unlike
    /// `retrieval`/`mcp`, no key rename is needed, so this reserializes the
    /// inner table directly rather than re-wrapping it under `web`), or
    /// `None` when absent — `web_fetch` registers regardless (it needs no
    /// backend); `web_search` simply never gets built.
    /// The `[minify]` section's *contents* (for `hotl_tools::MinifyConfig`),
    /// or `None` when absent — both keys default on, so absence and an empty
    /// section mean the same thing.
    pub fn minify_toml(&self) -> Option<String> {
        let minify = self.raw.as_ref()?.get("minify")?;
        toml::to_string(minify).ok()
    }

    pub fn web_toml(&self) -> Option<String> {
        let web = self.raw.as_ref()?.get("web")?;
        toml::to_string(web).ok()
    }

    /// The `[[hook]]` + `[diagnostics]` as a `hooks.toml`-shaped string, or None.
    pub fn hooks_toml(&self) -> Option<String> {
        let raw = self.raw.as_ref()?;
        let hooks = raw.get("hook");
        let diags = raw.get("diagnostics");
        if hooks.is_none() && diags.is_none() {
            return None;
        }
        let mut doc = toml::map::Map::new();
        if let Some(h) = hooks {
            doc.insert("hook".into(), h.clone());
        }
        if let Some(d) = diags {
            doc.insert("diagnostics".into(), d.clone());
        }
        toml::to_string(&toml::Value::Table(doc)).ok()
    }

    fn section_as_toml(&self, key: &str) -> Option<String> {
        let value = self.raw.as_ref()?.get(key)?;
        let mut doc = toml::map::Map::new();
        doc.insert(key.into(), value.clone());
        toml::to_string(&toml::Value::Table(doc)).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(toml: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), toml).unwrap();
        Config::load(dir.path())
    }

    #[test]
    fn window_resolves_from_the_catalog_when_unconfigured() {
        // The headline fix: the 1M-window default model stops compacting at
        // 160K because someone hardcoded 200_000.
        let (window, warning) = cfg_with("").context.resolve_window("claude-opus-4-8", None);
        assert_eq!(window, 1_000_000);
        assert!(warning.is_none());

        // A small model gets its small window, so default config stops
        // guaranteeing an overflow on it.
        let (window, _) = cfg_with("")
            .context
            .resolve_window("claude-haiku-4-5", None);
        assert_eq!(window, 200_000);
    }

    #[test]
    fn explicit_config_and_env_still_beat_the_catalog() {
        let cfg = cfg_with("[context]\nwindow = 64000\n");
        // config.toml over the catalog…
        assert_eq!(
            cfg.context.resolve_window("claude-opus-4-8", None).0,
            64_000
        );
        // …and env over config.toml (unchanged project-wide precedence).
        assert_eq!(
            cfg.context
                .resolve_window("claude-opus-4-8", Some("32000"))
                .0,
            32_000
        );
        // A garbage env value is ignored, not fatal — config still wins.
        assert_eq!(
            cfg.context
                .resolve_window("claude-opus-4-8", Some("banana"))
                .0,
            64_000
        );
    }

    #[test]
    fn an_uncatalogued_model_falls_back_loudly_not_silently() {
        let (window, warning) = cfg_with("").context.resolve_window("openai/llama3", None);
        assert_eq!(window, hotl_provider::catalog::FALLBACK_CONTEXT_WINDOW);
        let warning = warning.expect("must warn — a silent wrong window is the bug we are fixing");
        assert!(warning.contains("llama3"), "{warning}");
        assert!(
            warning.contains("[context] window"),
            "name the knob: {warning}"
        );
        // Configured explicitly: no warning, the user has answered the question.
        let cfg = cfg_with("[context]\nwindow = 8192\n");
        let (window, warning) = cfg.context.resolve_window("openai/llama3", None);
        assert_eq!(window, 8192);
        assert!(warning.is_none());
    }

    #[test]
    fn token_profile_follows_the_model() {
        let cfg = cfg_with("");
        // A catalogued model gets its own ratio…
        let opus = cfg.context.token_profile("claude-opus-4-8");
        assert_eq!(opus, hotl_context::TokenProfile::from_chars_per_token(3.0));
        // …and an unknown one gets the conservative default, never a guess.
        assert_eq!(
            cfg.context.token_profile("llama3"),
            hotl_context::TokenProfile::CONSERVATIVE
        );
    }

    #[test]
    fn default_model_matches_the_anthropic_provider() {
        // Pin, not a fix: `DEFAULT_MODEL` is defined three times
        // (catalog, hotl-provider-anthropic/src/lib.rs:23,
        // hotl-engine/src/lib.rs:80). The latter two belong to R3 and R2, who
        // have decision-log requests to re-export the catalog's. Until they
        // land, this fails loudly the moment the copies drift.
        assert_eq!(
            hotl_provider::catalog::DEFAULT_MODEL,
            hotl_provider_anthropic::DEFAULT_MODEL
        );
        // And the default model must itself be catalogued.
        assert!(hotl_provider::catalog::lookup(hotl_provider::catalog::DEFAULT_MODEL).is_some());
    }

    #[test]
    fn parses_settings_and_domain_sections() {
        let cfg = cfg_with(
            r#"
            [provider]
            model = "openai/gpt-5"
            base_url = "http://localhost:11434/v1"

            [context]
            window = 128000
            evict_tokens = 5000

            [behavior]
            vim_mode = false

            [retention]
            max_age_days = 30
            max_sessions = 100

            [[allow]]
            tool = "bash"
            prefix = "cargo "

            [[mcp]]
            name = "docs"
            command = "/bin/docs"
            description = "d"

            [[hook]]
            event = "pre_tool"
            command = "/bin/guard"

            [diagnostics]
            rs = "cargo check"
            "#,
        );
        assert_eq!(cfg.provider.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(cfg.context.window, Some(128_000));
        assert_eq!(cfg.behavior.vim_mode, Some(false));
        assert_eq!(cfg.retention.max_age_days, Some(30));
        assert_eq!(cfg.retention.max_sessions, Some(100));
        // Domain sections reserialize to their loaders' shapes.
        assert!(cfg.allow_toml().unwrap().contains("prefix = \"cargo \""));
        assert!(
            cfg.mcp_toml().unwrap().contains("[[server]]")
                && cfg.mcp_toml().unwrap().contains("docs")
        );
        // The parsed form the registry and `hotl mcp` share.
        let servers = cfg.mcp_servers();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "docs");
        assert_eq!(servers[0].command, "/bin/docs");
        let hooks = cfg.hooks_toml().unwrap();
        assert!(hooks.contains("pre_tool") && hooks.contains("cargo check"));
    }

    #[test]
    fn mcp_servers_is_empty_without_a_section() {
        assert!(cfg_with("[provider]\nmodel = \"x\"\n")
            .mcp_servers()
            .is_empty());
    }

    #[test]
    fn behavior_max_turns_parses_including_the_unlimited_sentinel() {
        assert_eq!(
            cfg_with("[behavior]\nmax_turns = 250\n").behavior.max_turns,
            Some(250)
        );
        // `-1` is the opt-in "no cap" posture — it must survive parsing as a
        // negative, not be rejected or clamped.
        assert_eq!(
            cfg_with("[behavior]\nmax_turns = -1\n").behavior.max_turns,
            Some(-1)
        );
        assert_eq!(cfg_with("").behavior.max_turns, None);
    }

    #[test]
    fn retrieval_section_reserializes_for_the_loader() {
        let cfg =
            cfg_with("[[retrieval]]\nname = \"notes\"\nkind = \"mcp\"\ncommand = \"/bin/rag\"\n");
        let t = cfg.retrieval_toml().unwrap();
        assert!(t.contains("[[backend]]") && t.contains("notes"));
        assert!(cfg_with("").retrieval_toml().is_none());
    }

    #[test]
    fn concurrency_section_parses_and_defaults_absent() {
        let cfg = cfg_with(
            "[concurrency]\nagents = 2\nrequests = 3\nsubprocs = 6\nworker_threads = 4\nblocking_threads = 32\n",
        );
        assert_eq!(cfg.concurrency.agents, Some(2));
        assert_eq!(cfg.concurrency.requests, Some(3));
        assert_eq!(cfg.concurrency.subprocs, Some(6));
        assert_eq!(cfg.concurrency.worker_threads, Some(4));
        assert_eq!(cfg.concurrency.blocking_threads, Some(32));
        let absent = cfg_with("");
        assert_eq!(absent.concurrency.agents, None);
        assert_eq!(absent.concurrency.requests, None);
        assert_eq!(absent.concurrency.subprocs, None);
    }

    #[test]
    fn minify_section_passes_through_and_is_absent_by_default() {
        let cfg = cfg_with("[minify]\nenable = false\nkeep_comments = false\n");
        let t = cfg.minify_toml().unwrap();
        assert!(t.contains("enable = false"), "was: {t}");
        assert!(t.contains("keep_comments = false"), "was: {t}");
        // Absent section: None — both keys default on, so absence and an empty
        // section mean the same thing.
        assert!(cfg_with("").minify_toml().is_none());
    }

    #[test]
    fn web_section_passes_through_for_the_search_tool_loader() {
        let cfg = cfg_with(
            "[web]\n[web.search]\nurl = \"https://s.example/api\"\napi_key_env = \"SEARCH_KEY\"\n",
        );
        let t = cfg.web_toml().unwrap();
        assert!(t.contains("[search]"), "was: {t}");
        assert!(t.contains("https://s.example/api"));
        assert!(t.contains("SEARCH_KEY"));
        // Absent [web] section: None — web_fetch registers regardless (it
        // needs no config), web_search simply never gets built.
        assert!(cfg_with("").web_toml().is_none());
    }

    #[test]
    fn provider_helper_keys_parse() {
        let cfg = cfg_with(
            "[provider]\napi_key_helper = \"mint-key --gw\"\napi_key_helper_ttl_secs = 300\n",
        );
        assert_eq!(
            cfg.provider.api_key_helper.as_deref(),
            Some("mint-key --gw")
        );
        assert_eq!(cfg.provider.api_key_helper_ttl_secs, Some(300));
    }

    #[test]
    fn network_egress_parses_and_unknown_fails_closed() {
        use hotl_tools::net::EgressPolicy;
        // Absent section: Open, no warning (the default is today's behavior).
        let (policy, warning) = cfg_with("").network.egress_policy();
        assert_eq!(policy, EgressPolicy::Open);
        assert!(warning.is_none());
        // Explicit modes.
        let (policy, warning) = cfg_with("[network]\negress = \"off\"\n")
            .network
            .egress_policy();
        assert_eq!(policy, EgressPolicy::Off);
        assert!(warning.is_none());
        let cfg = cfg_with(
            "[network]\negress = \"allowlist\"\nallow = [\"github.com\", \"*.crates.io\"]\n",
        );
        let (policy, warning) = cfg.network.egress_policy();
        assert_eq!(
            policy,
            EgressPolicy::Allowlist(vec!["github.com".into(), "*.crates.io".into()])
        );
        assert!(warning.is_none());
        // Unknown value: fail closed to Off, loudly — never open.
        let (policy, warning) = cfg_with("[network]\negress = \"opne\"\n")
            .network
            .egress_policy();
        assert_eq!(policy, EgressPolicy::Off);
        assert!(warning.unwrap().contains("opne"));
    }

    /// Shared harness for `[sandbox]` resolve tests: a scratch area holding a
    /// pretend config dir, data dir, and room for writable entries.
    fn sandbox_dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let scratch = tempfile::tempdir().unwrap();
        let config_dir = scratch.path().join("config-hotl");
        let data_dir = scratch.path().join("data-hotl");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        (scratch, config_dir, data_dir)
    }

    #[test]
    fn sandbox_section_parses_and_empty_resolves_to_nothing() {
        use hotl_tools::sandbox::FileToolsMode;
        let cfg = cfg_with("[sandbox]\nwritable = [\"/x\"]\nfile_tools = \"writable\"\n");
        assert_eq!(cfg.sandbox.writable, vec!["/x".to_string()]);
        assert_eq!(cfg.sandbox.file_tools.as_deref(), Some("writable"));

        // Absent section: exactly nothing — the floor is byte-identical to
        // an unconfigured hotl, and no warning rides every startup.
        let (_scratch, config_dir, data_dir) = sandbox_dirs();
        let (extras, warnings) = cfg_with("").sandbox.resolve(&config_dir, &data_dir);
        assert!(extras.writable.is_empty());
        assert_eq!(extras.file_tools, FileToolsMode::Workspace);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn sandbox_writable_creates_missing_dirs_and_canonicalizes() {
        let (scratch, config_dir, data_dir) = sandbox_dirs();
        let missing = scratch.path().join("bazel-cache").join("disk");
        let real = scratch.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = scratch.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let cfg = SandboxCfg {
            writable: vec![
                format!("{}/", missing.display()),        // trailing slash
                link.join("inner").display().to_string(), // via a symlink
            ],
            readable: Vec::new(),
            file_tools: None,
        };
        let (extras, warnings) = cfg.resolve(&config_dir, &data_dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(missing.is_dir(), "missing entries are created at startup");
        assert_eq!(extras.writable.len(), 2);
        assert_eq!(extras.writable[0], missing.canonicalize().unwrap());
        // The symlink is resolved: the grant lands on the target, so the
        // kernel and the tool boundary agree on one canonical name.
        assert_eq!(
            extras.writable[1],
            real.join("inner").canonicalize().unwrap()
        );
    }

    #[test]
    fn sandbox_writable_refuses_config_and_data_dirs() {
        let (scratch, config_dir, data_dir) = sandbox_dirs();
        let good = scratch.path().join("cache");
        let link_to_config = scratch.path().join("innocent");
        std::os::unix::fs::symlink(&config_dir, &link_to_config).unwrap();
        let cfg = SandboxCfg {
            writable: vec![
                config_dir.display().to_string(),             // is the config dir
                config_dir.join("sub").display().to_string(), // inside it
                scratch.path().display().to_string(),         // contains it
                data_dir.display().to_string(),               // the data dir
                link_to_config.display().to_string(),         // symlink-smuggled
                good.display().to_string(),                   // the survivor
            ],
            readable: Vec::new(),
            file_tools: None,
        };
        let (extras, warnings) = cfg.resolve(&config_dir, &data_dir);
        // Refusal wins, entry by entry — the good sibling still lands.
        assert_eq!(extras.writable, vec![good.canonicalize().unwrap()]);
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        for w in &warnings {
            assert!(w.contains("refused"), "{w}");
        }
        // Nothing was created inside the config dir on the way.
        assert!(!config_dir.join("sub").exists());
    }

    #[test]
    fn sandbox_writable_warns_but_honors_risky_roots() {
        let (_scratch, config_dir, data_dir) = sandbox_dirs();
        let cfg = SandboxCfg {
            writable: vec!["/etc".to_string()],
            readable: Vec::new(),
            file_tools: None,
        };
        let (extras, warnings) = cfg.resolve(&config_dir, &data_dir);
        assert_eq!(
            extras.writable,
            vec![std::path::Path::new("/etc").canonicalize().unwrap()]
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("/etc"), "{}", warnings[0]);
        assert!(!warnings[0].contains("refused"), "{}", warnings[0]);
    }

    #[test]
    fn sandbox_writable_skips_files_and_relative_entries() {
        let (scratch, config_dir, data_dir) = sandbox_dirs();
        let file = scratch.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let cfg = SandboxCfg {
            writable: vec![
                file.display().to_string(),
                "relative/cache".to_string(),
                "~".to_string(), // bare ~ is not expanded — stays relative
            ],
            readable: Vec::new(),
            file_tools: None,
        };
        let (extras, warnings) = cfg.resolve(&config_dir, &data_dir);
        assert!(extras.writable.is_empty());
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("not a directory"), "{}", warnings[0]);
        assert!(warnings[1].contains("absolute"), "{}", warnings[1]);
        assert!(warnings[2].contains("absolute"), "{}", warnings[2]);
    }

    #[test]
    fn sandbox_file_tools_parses_and_unknown_fails_closed() {
        use hotl_tools::sandbox::FileToolsMode;
        let (scratch, config_dir, data_dir) = sandbox_dirs();
        let cache = scratch.path().join("cache");
        let with_mode = |mode: Option<&str>, writable: Vec<String>| {
            SandboxCfg {
                writable,
                readable: Vec::new(),
                file_tools: mode.map(String::from),
            }
            .resolve(&config_dir, &data_dir)
        };
        let entry = vec![cache.display().to_string()];

        let (extras, w) = with_mode(Some("workspace"), entry.clone());
        assert_eq!(extras.file_tools, FileToolsMode::Workspace);
        assert!(w.is_empty(), "{w:?}");

        let (extras, w) = with_mode(Some("writable"), entry.clone());
        assert_eq!(extras.file_tools, FileToolsMode::Writable);
        assert!(w.is_empty(), "{w:?}");

        // Unknown mode fails closed to workspace, naming the accepted values.
        let (extras, w) = with_mode(Some("writeable"), entry);
        assert_eq!(extras.file_tools, FileToolsMode::Workspace);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("writeable") && w[0].contains("workspace | writable"),
            "{}",
            w[0]
        );

        // "writable" with nothing writable is a no-op worth saying out loud.
        let (extras, w) = with_mode(Some("writable"), vec![]);
        assert_eq!(extras.file_tools, FileToolsMode::Writable);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("no effect"), "{}", w[0]);
    }

    /// Drive the read-carve seam with an explicit `$HOME` and write set, so
    /// the Tier B list is exactly the credential list and nothing depends on
    /// where the test binary happens to be running.
    fn read_deny(
        cfg: &SandboxCfg,
        config_dir: &Path,
        data_dir: &Path,
        home: &Path,
        granted: &[std::path::PathBuf],
    ) -> (hotl_tools::sandbox::ReadDeny, Vec<String>) {
        let mut warnings = Vec::new();
        let deny = cfg.resolve_read_deny(config_dir, data_dir, granted, Some(home), &mut warnings);
        (deny, warnings)
    }

    /// The Tier A / Tier B split, and the fact that Tier A is not config.
    #[test]
    fn read_deny_denies_hotls_own_dirs_and_the_credential_class() {
        let (scratch, config_dir, data_dir) = sandbox_dirs();
        let home = scratch.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let home = home.canonicalize().unwrap();
        let (deny, warnings) = read_deny(
            &SandboxCfg::default(),
            &config_dir,
            &data_dir,
            &home,
            &[std::path::PathBuf::from("/dev")],
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        // Tier A: hotl's own dirs. The run dir lives under the data dir, so
        // denying the data dir is what covers the session token.
        assert_eq!(
            deny.always,
            vec![
                config_dir.canonicalize().unwrap(),
                data_dir.canonicalize().unwrap()
            ]
        );
        // Tier B: every credential entry, whether or not it exists today.
        for name in [
            ".ssh",
            ".aws",
            ".config/gcloud",
            ".azure",
            ".npmrc",
            ".pypirc",
            ".netrc",
            ".dockercfg",
        ] {
            assert!(
                deny.secrets.contains(&home.join(name)),
                "{name} missing from {:?}",
                deny.secrets
            );
        }
    }

    #[test]
    fn readable_entry_touching_hotls_own_dirs_is_refused() {
        let (scratch, config_dir, data_dir) = sandbox_dirs();
        let home = scratch.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let cfg = SandboxCfg {
            readable: vec![
                config_dir.display().to_string(),           // is the config dir
                data_dir.join("run").display().to_string(), // inside the data dir
                scratch.path().display().to_string(),       // contains both
                config_dir.join("x").display().to_string(), // under the config dir
            ],
            ..SandboxCfg::default()
        };
        let (deny, warnings) = read_deny(&cfg, &config_dir, &data_dir, &home, &[]);
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        for w in &warnings {
            assert!(w.contains("refused"), "{w}");
        }
        // Tier A survives every attempt to name it.
        assert_eq!(deny.always.len(), 2);
    }

    #[test]
    fn readable_lifts_only_the_named_subtree() {
        let (scratch, config_dir, data_dir) = sandbox_dirs();
        let home = scratch.path().join("home");
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let home = home.canonicalize().unwrap();
        let cfg = SandboxCfg {
            readable: vec![home.join(".aws").display().to_string()],
            ..SandboxCfg::default()
        };
        let (deny, warnings) = read_deny(&cfg, &config_dir, &data_dir, &home, &[]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!deny.secrets.contains(&home.join(".aws")), "{deny:?}");
        assert!(deny.secrets.contains(&home.join(".ssh")), "{deny:?}");
    }

    /// `writable` creates missing dirs because Landlock can only grant on an
    /// existing path. `readable` must not: a read-deny for a path that does
    /// not exist is already a no-op, and conjuring `~/.ssh` out of a config
    /// parse would be surprising and wrong.
    #[test]
    fn readable_does_not_create_missing_directories() {
        let (scratch, config_dir, data_dir) = sandbox_dirs();
        let home = scratch.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let missing = home.join(".ssh");
        let cfg = SandboxCfg {
            readable: vec![missing.display().to_string()],
            ..SandboxCfg::default()
        };
        let (_deny, warnings) = read_deny(&cfg, &config_dir, &data_dir, &home, &[]);
        assert!(!missing.exists(), "readable must never mkdir");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("does not exist"), "{}", warnings[0]);
    }

    /// Landlock resolves against the closest matching rule, so a write grant
    /// on an ancestor re-opens the read. Such an entry is dropped loudly
    /// rather than shipped as a denial the kernel will not deliver.
    #[test]
    fn read_deny_drops_an_entry_that_sits_inside_a_write_grant() {
        let (scratch, config_dir, data_dir) = sandbox_dirs();
        let home = scratch.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let home = home.canonicalize().unwrap();
        // Granting write on $HOME puts every credential path inside it.
        let (deny, warnings) = read_deny(
            &SandboxCfg::default(),
            &config_dir,
            &data_dir,
            &home,
            std::slice::from_ref(&home),
        );
        assert!(deny.secrets.is_empty(), "{:?}", deny.secrets);
        assert!(
            warnings.iter().any(|w| w.contains("cannot deny reads of")),
            "{warnings:?}"
        );
        // Tier A is unaffected: it is never inside a writable root (refused).
        assert_eq!(deny.always.len(), 2);
    }

    #[test]
    fn permissions_mode_resolves_with_env_and_fails_closed() {
        use hotl_tools::rules::PermissionMode;
        // Absent → the product default: bypass, plan off, no warning.
        let r = cfg_with("").permissions.resolve(None, None);
        assert_eq!(r.mode, PermissionMode::Bypass);
        assert!(!r.plan);
        assert!(r.warning.is_none());
        // Explicit ask.
        let r = cfg_with("[permissions]\nmode = \"ask\"\n")
            .permissions
            .resolve(None, None);
        assert_eq!(r.mode, PermissionMode::Ask);
        // Env beats config.
        let r = cfg_with("[permissions]\nmode = \"ask\"\n")
            .permissions
            .resolve(Some("bypass"), None);
        assert_eq!(r.mode, PermissionMode::Bypass);
        // Typo fails closed to ask, loudly — never silently bypass.
        let r = cfg_with("[permissions]\nmode = \"byapss\"\n")
            .permissions
            .resolve(None, None);
        assert_eq!(r.mode, PermissionMode::Ask);
        assert!(r.warning.unwrap().contains("byapss"));
        // dontask parses from config and from HOTL_PERMISSIONS.
        let r = cfg_with("[permissions]\nmode = \"dontask\"\n")
            .permissions
            .resolve(None, None);
        assert_eq!(r.mode, PermissionMode::DontAsk);
        assert!(r.warning.is_none());
        let r = cfg_with("").permissions.resolve(Some("dontask"), None);
        assert_eq!(r.mode, PermissionMode::DontAsk);
    }

    /// `auto` was renamed `bypass`, and `plan` stopped being a mode at all.
    /// Both spellings still appear in configs and session logs in the wild.
    #[test]
    fn legacy_mode_words_still_resolve() {
        use hotl_tools::rules::PermissionMode;
        for src in [
            "[permissions]\nmode = \"auto\"\n",
            "[permissions]\nmode = \"AUTO\"\n",
        ] {
            let r = cfg_with(src).permissions.resolve(None, None);
            assert_eq!(r.mode, PermissionMode::Bypass, "{src}");
            assert!(!r.plan);
            assert!(r.warning.is_none(), "a legacy name is not a typo");
        }
        // `plan` turns the overlay on and leaves the mode at its default —
        // it never named one, so it must not fail closed as a typo would.
        let r = cfg_with("[permissions]\nmode = \"plan\"\n")
            .permissions
            .resolve(None, None);
        assert_eq!(r.mode, PermissionMode::Bypass);
        assert!(r.plan);
        assert!(r.warning.is_none());
        let r = cfg_with("").permissions.resolve(Some("plan"), None);
        assert!(r.plan);
    }

    #[test]
    fn plan_resolves_independently_of_mode() {
        use hotl_tools::rules::PermissionMode;
        // The two axes compose: an explicit plan flag rides any mode.
        let r = cfg_with("[permissions]\nmode = \"dontask\"\nplan = true\n")
            .permissions
            .resolve(None, None);
        assert_eq!(r.mode, PermissionMode::DontAsk);
        assert!(r.plan);
        // HOTL_PLAN beats the config key, in both directions.
        let r = cfg_with("[permissions]\nplan = true\n")
            .permissions
            .resolve(None, Some("0"));
        assert!(!r.plan);
        let r = cfg_with("[permissions]\nplan = false\n")
            .permissions
            .resolve(None, Some("1"));
        assert!(r.plan);
        // A bare `HOTL_PLAN=` reads as "asked for it", not as unset.
        assert!(cfg_with("").permissions.resolve(None, Some("yes")).plan);
        assert!(!cfg_with("").permissions.resolve(None, Some("false")).plan);
    }

    #[test]
    fn vim_mode_parses_and_defaults() {
        let cfg = cfg_with("[behavior]\nvim_mode = false\n");
        assert_eq!(cfg.behavior.vim_mode, Some(false));
        assert_eq!(cfg_with("").behavior.vim_mode, None);
    }

    #[test]
    fn vim_mode_resolves_off_unless_opted_in() {
        // Absent section, and an explicit `false`: plain insert-mode editing.
        assert!(!cfg_with("").behavior.vim_mode());
        assert!(!cfg_with("[behavior]\nvim_mode = false\n")
            .behavior
            .vim_mode());
        // Only an explicit `true` turns the modal editor on.
        assert!(cfg_with("[behavior]\nvim_mode = true\n")
            .behavior
            .vim_mode());
    }

    #[test]
    fn mouse_parses_and_resolves_on_unless_opted_out() {
        assert_eq!(cfg_with("").behavior.mouse, None);
        assert!(cfg_with("").behavior.mouse(), "capture is on by default");
        assert!(!cfg_with("[behavior]\nmouse = false\n").behavior.mouse());
    }

    #[test]
    fn copy_on_select_parses_and_resolves_on_unless_opted_out() {
        assert_eq!(cfg_with("").behavior.copy_on_select, None);
        assert!(
            cfg_with("").behavior.copy_on_select(),
            "drag-to-copy is on by default"
        );
        assert!(!cfg_with("[behavior]\ncopy_on_select = false\n")
            .behavior
            .copy_on_select());
    }

    #[test]
    fn history_parses_and_falls_back_to_defaults() {
        // Absent section: the product defaults (on, 1000 entries, 2 MiB).
        let cfg = cfg_with("");
        assert!(cfg.history.is_enabled());
        assert_eq!(cfg.history.max_entries(), 1000);
        assert_eq!(cfg.history.max_bytes(), 2 * 1024 * 1024);

        // Explicit values override.
        let cfg = cfg_with(
            "[history]\nenabled = false\nmax_entries = 5\nmax_bytes = 4096\npath = \"~/h.jsonl\"\n",
        );
        assert!(!cfg.history.is_enabled());
        assert_eq!(cfg.history.max_entries(), 5);
        assert_eq!(cfg.history.max_bytes(), 4096);
    }

    #[test]
    fn history_path_expands_home_and_defaults_under_data_dir() {
        let data = std::path::Path::new("/data/hotl");
        // No path → <data-dir>/history.jsonl.
        assert_eq!(
            cfg_with("").history.resolved_path(data),
            data.join("history.jsonl")
        );
        // A `~/`-path expands against HOME.
        let cfg = cfg_with("[history]\npath = \"~/notes/h.jsonl\"\n");
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(cfg.history.resolved_path(data), home.join("notes/h.jsonl"));
    }

    #[test]
    fn skills_marketplaces_parse_and_resolve() {
        let cfg = cfg_with(
            "[skills.marketplaces]\n\
             acme = \"https://github.com/acme/skills.git\"\n\
             team = \"/abs/team-skills\"\n\
             \"bad:name\" = \"/x\"\n",
        );
        let dir = std::path::Path::new("/cfg");
        let (roots, warnings) = cfg.skills.marketplace_roots(dir);
        assert_eq!(
            roots,
            vec![
                ("acme".to_string(), dir.join("marketplaces/acme")),
                (
                    "team".to_string(),
                    std::path::PathBuf::from("/abs/team-skills")
                ),
            ]
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("bad:name"), "{warnings:?}");

        // `~/` expands against HOME; absent section resolves empty.
        let cfg = cfg_with("[skills.marketplaces]\nhome = \"~/team-skills\"\n");
        let (roots, _) = cfg.skills.marketplace_roots(dir);
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(roots, vec![("home".to_string(), home.join("team-skills"))]);
        assert!(cfg_with("").skills.marketplace_roots(dir).0.is_empty());
    }

    #[test]
    fn git_url_detection() {
        for url in [
            "https://github.com/a/b.git",
            "http://host/repo",
            "git@github.com:a/b.git",
            "ssh://host/repo",
            "/local/path/origin.git",
        ] {
            assert!(is_git_url(url), "{url}");
        }
        for path in ["~/skills", "/abs/dir", "relative/dir"] {
            assert!(!is_git_url(path), "{path}");
        }
    }

    #[test]
    fn empty_or_absent_config_is_defaults_and_none_sections() {
        let cfg = Config::load(std::path::Path::new("/no/such/dir"));
        assert!(cfg.provider.model.is_none());
        assert!(
            cfg.allow_toml().is_none() && cfg.mcp_toml().is_none() && cfg.hooks_toml().is_none()
        );
        assert!(cfg.retention.max_age_days.is_none());
    }
}
