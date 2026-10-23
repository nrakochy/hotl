//! The feature-stage registry (0076 §C): every user-facing switch, the stage
//! it is at, and since when. Reads stay where they are — this table is what
//! `hotl doctor`, `hotl features` and the docs page say about them. No flag
//! changes behaviour by being listed here.

/// What a stage promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// May change or go without notice.
    Experimental,
    /// Shipped; the shape may still move, and it will get a config twin.
    Beta,
    /// Covered by the distribution contract's Tier-1/2 promise.
    Stable,
    /// Works; a named replacement exists; removal is announced a minor ahead.
    Deprecated,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Experimental => "experimental",
            Stage::Beta => "beta",
            Stage::Stable => "stable",
            Stage::Deprecated => "deprecated",
        }
    }
}

/// How a feature is switched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    Env(&'static str),
    /// `table.key` in config.toml.
    Config(&'static str),
    EnvOrConfig {
        env: &'static str,
        config: &'static str,
    },
}

impl Switch {
    pub fn env(self) -> Option<&'static str> {
        match self {
            Switch::Env(e) | Switch::EnvOrConfig { env: e, .. } => Some(e),
            Switch::Config(_) => None,
        }
    }

    pub fn config(self) -> Option<&'static str> {
        match self {
            Switch::Config(c) | Switch::EnvOrConfig { config: c, .. } => Some(c),
            Switch::Env(_) => None,
        }
    }

    pub fn label(self) -> String {
        match self {
            Switch::Env(e) => e.to_string(),
            Switch::Config(c) => format!("[{c}]"),
            Switch::EnvOrConfig { env, config } => format!("{env} / [{config}]"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Feature {
    pub name: &'static str,
    pub stage: Stage,
    pub switch: Switch,
    /// The first release that carried it (`0.29.0` = the next one).
    pub since: &'static str,
    /// Docs page and anchor, relative to the site root.
    pub doc: &'static str,
    pub summary: &'static str,
    /// What to use instead, for a `Deprecated` row.
    pub replacement: Option<&'static str>,
}

const ENV_DOC: &str = "configuration/#environment-variables";

const fn env(
    name: &'static str,
    stage: Stage,
    var: &'static str,
    since: &'static str,
    summary: &'static str,
) -> Feature {
    Feature {
        name,
        stage,
        switch: Switch::Env(var),
        since,
        doc: ENV_DOC,
        summary,
        replacement: None,
    }
}

const fn config(
    name: &'static str,
    key: &'static str,
    since: &'static str,
    doc: &'static str,
    summary: &'static str,
) -> Feature {
    Feature {
        name,
        stage: Stage::Experimental,
        switch: Switch::Config(key),
        since,
        doc,
        summary,
        replacement: None,
    }
}

use Stage::{Beta, Deprecated, Experimental, Stable};

/// The table, in the order the docs page prints it.
pub const FEATURES: &[Feature] = &[
    env(
        "model",
        Stable,
        "HOTL_MODEL",
        "0.2.0",
        "the session model (`provider/model`)",
    ),
    env(
        "provider endpoint",
        Stable,
        "HOTL_PROVIDER_BASE_URL",
        "0.12.0",
        "the active provider's base URL",
    ),
    env(
        "provider auth",
        Stable,
        "HOTL_PROVIDER_AUTH",
        "0.4.0",
        "`api_key` or `subscription`",
    ),
    env(
        "key helper",
        Stable,
        "HOTL_API_KEY_HELPER",
        "0.2.0",
        "a command whose stdout is the API key",
    ),
    env(
        "key helper TTL",
        Stable,
        "HOTL_API_KEY_HELPER_TTL_SECS",
        "0.2.0",
        "re-run the key helper after this long",
    ),
    env(
        "context window",
        Stable,
        "HOTL_CONTEXT_WINDOW",
        "0.2.0",
        "override the catalogued window",
    ),
    env(
        "fast model",
        Stable,
        "HOTL_FAST_MODEL",
        "0.2.0",
        "housekeeping model",
    ),
    env(
        "utility model",
        Stable,
        "HOTL_UTILITY_MODEL",
        "0.26.0",
        "model for digests and goal checks",
    ),
    env("effort", Stable, "HOTL_EFFORT", "0.12.0", "reasoning depth"),
    env(
        "evict threshold",
        Stable,
        "HOTL_EVICT_TOKENS",
        "0.2.0",
        "spill tool results larger than this",
    ),
    env(
        "permission mode",
        Stable,
        "HOTL_PERMISSIONS",
        "0.2.0",
        "`ask` | `bypass` | `dontask`",
    ),
    env(
        "plan mode",
        Stable,
        "HOTL_PLAN",
        "0.9.0",
        "start in plan mode",
    ),
    env(
        "sandbox",
        Stable,
        "HOTL_SANDBOX",
        "0.2.0",
        "`off` disables the bash floor",
    ),
    env(
        "turn budget",
        Stable,
        "HOTL_MAX_TURNS",
        "0.5.1",
        "model steps per prompt",
    ),
    env(
        "tool-call cap",
        Stable,
        "HOTL_MAX_TOOL_CALLS",
        "0.26.0",
        "tool calls per session",
    ),
    env(
        "cost cap",
        Stable,
        "HOTL_MAX_COST_USD",
        "0.26.0",
        "USD per session",
    ),
    env(
        "output cap",
        Stable,
        "HOTL_MAX_TOKENS",
        "0.14.0",
        "per-sample output tokens",
    ),
    env(
        "request concurrency",
        Stable,
        "HOTL_CONCURRENCY_REQUESTS",
        "0.5.0",
        "concurrent provider requests",
    ),
    env(
        "agent concurrency",
        Stable,
        "HOTL_CONCURRENCY_AGENTS",
        "0.5.0",
        "concurrent sub-agents",
    ),
    env(
        "blocking threads",
        Stable,
        "HOTL_CONCURRENCY_BLOCKING_THREADS",
        "0.5.0",
        "blocking-pool size",
    ),
    env(
        "mouse",
        Stable,
        "HOTL_MOUSE",
        "0.5.2",
        "mouse capture in the console",
    ),
    env(
        "env scrub",
        Stable,
        "HOTL_SCRUB_ENV",
        "0.5.2",
        "extra env names scrubbed from children",
    ),
    env(
        "probe dir",
        Stable,
        "HOTL_SANDBOX_PROBE_DIR",
        "0.5.2",
        "where the sandbox probe writes",
    ),
    env(
        "shell",
        Stable,
        "HOTL_SHELL",
        "0.12.0",
        "the POSIX shell `bash` runs",
    ),
    env(
        "preapproved rules",
        Stable,
        "HOTL_PREAPPROVED",
        "0.2.0",
        "admin preapproved-rules file",
    ),
    env(
        "thinking",
        Beta,
        "HOTL_THINKING",
        "0.5.2",
        "extended thinking on/off (config key pending)",
    ),
    env(
        "strict env scrub",
        Beta,
        "HOTL_SCRUB_ENV_STRICT",
        "0.5.2",
        "heuristic scrub of secret-looking env",
    ),
    env(
        "reset compaction",
        Beta,
        "HOTL_COMPACTION_RESET",
        "0.2.0",
        "fresh-slate folds",
    ),
    env(
        "hide context %",
        Beta,
        "HOTL_HIDE_CONTEXT_PCT",
        "0.2.0",
        "keep fullness out of the turn context",
    ),
    env(
        "program status",
        Beta,
        "HOTL_PROGRAM_STATUS",
        "0.29.0",
        "terminal progress/status sequences",
    ),
    env(
        "unix sockets",
        Experimental,
        "HOTL_UNIX_SOCKETS",
        "0.5.2",
        "let sandboxed bash reach unix sockets",
    ),
    env(
        "macOS automation",
        Experimental,
        "HOTL_MACOS_AUTOMATION",
        "0.5.2",
        "let sandboxed bash drive Apple Events",
    ),
    env(
        "cloud metadata",
        Experimental,
        "HOTL_WEB_ALLOW_METADATA",
        "0.5.2",
        "let web_fetch reach metadata addresses",
    ),
    env(
        "proxy auth",
        Experimental,
        "HOTL_PROXY_AUTH",
        "0.5.2",
        "egress proxy credential mode",
    ),
    env(
        "subprocess permits",
        Experimental,
        "HOTL_CONCURRENCY_SUBPROCS",
        "0.5.0",
        "reserved; parsed but not wired",
    ),
    env(
        "worker threads",
        Experimental,
        "HOTL_CONCURRENCY_WORKER_THREADS",
        "0.5.0",
        "reserved; parsed but not wired",
    ),
    env(
        "shell trace",
        Experimental,
        "HOTL_SHELL_TRACE",
        "0.29.0",
        "zsh plugin: attach the last command to `: `",
    ),
    env(
        "shell trace lines",
        Experimental,
        "HOTL_TRACE_LINES",
        "0.29.0",
        "zsh plugin: lines of output captured",
    ),
    env(
        "shell trace age",
        Experimental,
        "HOTL_TRACE_MAX_AGE",
        "0.29.0",
        "zsh plugin: oldest trace attached",
    ),
    env(
        "prompt height",
        Experimental,
        "HOTL_PROMPT_LINES",
        "0.29.0",
        "zsh plugin: rows your prompt takes",
    ),
    Feature {
        name: "OpenAI base URL (legacy)",
        stage: Deprecated,
        switch: Switch::Env("HOTL_OPENAI_BASE_URL"),
        since: "0.2.0",
        doc: ENV_DOC,
        summary: "alias of the provider base URL",
        replacement: Some("HOTL_PROVIDER_BASE_URL"),
    },
    Feature {
        name: "Anthropic base URL (legacy)",
        stage: Deprecated,
        switch: Switch::Env("HOTL_ANTHROPIC_BASE_URL"),
        since: "0.4.0",
        doc: ENV_DOC,
        summary: "alias of the provider base URL",
        replacement: Some("HOTL_PROVIDER_BASE_URL"),
    },
    Feature {
        name: "cache keep-alive",
        stage: Experimental,
        switch: Switch::EnvOrConfig {
            env: "HOTL_CACHE_WARMING",
            config: "provider.cache_warming",
        },
        since: "0.29.0",
        doc: "configuration/#session-budgets-behavior-max_tool_calls-max_cost_usd",
        summary: "keep the 5-minute prompt cache alive while a tool runs",
        replacement: None,
    },
    config(
        "OpenAI cache breakpoints",
        "provider.cache_breakpoints",
        "0.24.0",
        "gateway/",
        "explicit GPT-5.6 prompt-cache breakpoints",
    ),
    config(
        "sub-agent depth",
        "agents.max_depth",
        "0.29.0",
        "agents/",
        "how deep spawned agents may nest",
    ),
    config(
        "sub-agent count",
        "agents.max_children",
        "0.29.0",
        "agents/",
        "running plus parked sub-agents",
    ),
    config(
        "background jobs",
        "behavior.max_jobs",
        "0.29.0",
        "configuration/",
        "running background jobs per session",
    ),
];

/// Names that start with `HOTL_` but are not user switches: constants, the
/// variable hotl sets for hooks, and test hooks. Entries ending in `_` are
/// prefixes. The drift test tolerates exactly these.
#[cfg_attr(not(test), allow(dead_code))] // read by the drift test below
pub const INTERNAL_NAMES: &[&str] = &[
    "HOTL_HOOK_EVENT",
    "HOTL_SBX_REFUSED",
    "HOTL_ASK_TAIL",
    "HOTL_NON_AGENT_SUBCOMMANDS",
    "HOTL_FSGUARD_FORCE_DESCEND",
    "HOTL_BLOB_SECRET",
    "HOTL_UPDATE_LOOP_BASELINE",
    "HOTL_TMUX_SMOKE",
    "HOTL_T8_",
    "HOTL_TEST_",
    "HOTL_CONCURRENCY_",
];

/// The docs page's generated block: one markdown row per feature, in order.
pub fn markdown() -> String {
    let mut out = String::from("| Switch | Stage | Since | What |\n|---|---|---|---|\n");
    for f in FEATURES {
        let what = match f.replacement {
            Some(r) => format!("{} — use `{r}`", f.summary),
            None => f.summary.to_string(),
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {what} |\n",
            f.switch.label(),
            f.stage.as_str(),
            f.since
        ));
    }
    out
}

/// The non-stable features switched on right now: a set env var, or a
/// config key present. Pure over its lookups, so a test needs no real env.
pub fn set_switches(
    features: &[Feature],
    env_get: impl Fn(&str) -> Option<String>,
    config_has: impl Fn(&str) -> bool,
) -> Vec<(&Feature, String)> {
    features
        .iter()
        .filter(|f| f.stage != Stage::Stable)
        .filter_map(|f| {
            if let Some(v) = f.switch.env().and_then(|e| env_get(e).map(|v| (e, v))) {
                return Some((f, format!("{}={}", v.0, v.1)));
            }
            f.switch
                .config()
                .filter(|c| config_has(c))
                .map(|c| (f, format!("[{c}]")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src_root() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("..")
    }

    /// `HOTL_` + `[A-Z0-9_]+` in every `crates/*/src/**/*.rs`, hand-scanned (no
    /// regex crate in the tree).
    fn scan() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        for c in std::fs::read_dir(src_root()).unwrap().flatten() {
            let src = c.path().join("src");
            if src.is_dir() {
                walk(&src, &mut files);
            }
        }
        let mut names = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let b = text.as_bytes();
            let mut i = 0;
            while let Some(at) = text[i..].find("HOTL_") {
                let start = i + at;
                let mut end = start + 5;
                while end < b.len()
                    && (b[end].is_ascii_uppercase() || b[end].is_ascii_digit() || b[end] == b'_')
                {
                    end += 1;
                }
                // A bare `HOTL_` is prose ("every `HOTL_*` name"), not a switch.
                if end > start + 5 {
                    names.push((text[start..end].to_string(), f.display().to_string()));
                }
                i = end;
            }
        }
        names
    }

    fn internal(name: &str) -> bool {
        INTERNAL_NAMES.iter().any(|n| {
            if n.ends_with('_') {
                name.starts_with(n)
            } else {
                name == *n
            }
        })
    }

    #[test]
    fn every_hotl_name_in_the_source_is_registered_or_internal() {
        let registered: Vec<&str> = FEATURES.iter().filter_map(|f| f.switch.env()).collect();
        let mut missing: Vec<String> = scan()
            .into_iter()
            .filter(|(n, _)| !registered.contains(&n.as_str()) && !internal(n))
            .map(|(n, f)| format!("{n} ({f})"))
            .collect();
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "unregistered HOTL_ names: {missing:#?}");
    }

    #[test]
    fn every_registered_env_switch_is_read_somewhere() {
        let found = scan();
        for f in FEATURES {
            if let Some(e) = f.switch.env() {
                assert!(
                    found
                        .iter()
                        .any(|(n, file)| n == e && !file.ends_with("features.rs")),
                    "{e} is registered but nothing reads it"
                );
            }
        }
    }

    #[test]
    fn every_row_has_a_since_and_a_doc_link() {
        for f in FEATURES {
            assert!(
                f.since.split('.').count() == 3,
                "{}: since {}",
                f.name,
                f.since
            );
            assert!(!f.doc.is_empty() && !f.doc.starts_with('/'), "{}", f.name);
            assert!(!f.summary.is_empty(), "{}", f.name);
        }
    }

    #[test]
    fn deprecated_rows_name_a_replacement() {
        for f in FEATURES {
            assert_eq!(
                f.stage == Stage::Deprecated,
                f.replacement.is_some(),
                "{}: a replacement is named exactly when deprecated",
                f.name
            );
        }
    }

    #[test]
    fn set_switches_reports_only_non_stable_switches_that_are_on() {
        let env = |k: &str| match k {
            "HOTL_MACOS_AUTOMATION" => Some("allow".to_string()),
            "HOTL_MODEL" => Some("m".to_string()),
            _ => None,
        };
        let on = set_switches(FEATURES, env, |c| c == "agents.max_depth");
        let names: Vec<&str> = on.iter().map(|(f, _)| f.name).collect();
        assert_eq!(names, ["macOS automation", "sub-agent depth"]);
        assert_eq!(on[0].1, "HOTL_MACOS_AUTOMATION=allow");
    }

    #[test]
    fn features_markdown_has_one_row_per_feature_in_table_order() {
        let md = markdown();
        let rows: Vec<&str> = md.lines().skip(2).collect();
        assert_eq!(rows.len(), FEATURES.len());
        assert!(
            rows[0].starts_with("| `HOTL_MODEL` | stable | 0.2.0 |"),
            "{}",
            rows[0]
        );
        assert!(md.contains("| `HOTL_OPENAI_BASE_URL` | deprecated | 0.2.0 | alias of the provider base URL — use `HOTL_PROVIDER_BASE_URL` |"));
    }
}
