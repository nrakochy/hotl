//! First-run wizard (`hotl setup`) and update check (`hotl update`) — MD.
//!
//! `setup` writes the default config the "defaults are the safety design"
//! belief depends on, but **never silently**: it's an explicit subcommand and
//! it refuses to overwrite an existing file without `--force`. Bare `hotl`
//! prints a one-line hint on first run rather than writing anything itself.

use std::path::Path;

/// The shipped default allow-rules. Read-only
/// conveniences only; everything that runs code or writes files is commented
/// out for the owner to enable deliberately.
pub const DEFAULT_CONFIG: &str = "\
# ~/.config/hotl/config.toml — the single hotl config file.
# Docs: https://nrakochy.github.io/hotl/
# Every hand-editable setting lives here. Env vars override these (HOTL_MODEL,
# ANTHROPIC_API_KEY / OPENAI_API_KEY, HOTL_OPENAI_BASE_URL,
# HOTL_ANTHROPIC_BASE_URL, HOTL_PROVIDER_AUTH, HOTL_SANDBOX=off).

[provider]
# provider/model. `openai/…` covers any OpenAI-compatible endpoint.
# model = \"openai/gpt-5\"
# base_url = \"http://localhost:11434/v1\"   # endpoint for the active provider
# auth = \"subscription\"                    # endpoint authenticates for you;
#                                          # hotl holds no key (needs base_url)
# fast_model = \"...\"                        # cheap model for compaction summaries
# Run a command to obtain the API key (stdout = key). Beats the static env
# key when set. For gateways with short-lived keys, see
# https://nrakochy.github.io/hotl/gateway/
# api_key_helper = \"my-mint-key\"
# api_key_helper_ttl_secs = 300

[context]
# window = 200000            # your model's context size, in tokens
# evict_tokens = 20000       # offload tool results larger than this (0 disables)
# compaction_reset = false   # fresh-slate compaction instead of in-place
# show_used_pct = true       # show context-fullness in each turn's status

[behavior]
# sandbox = true             # false disables the bash sandbox floor
# vim_mode = true            # vim-style keys in the console's input editor

[permissions]
mode = \"auto\"   # no per-action y/N; protected paths + sandbox still guard.
                # \"ask\" = approve every mutating/executing call. A
                # security-enforced build ignores this key entirely.

[skills]
# claude = true   # false stops reading Claude Code skills (~/.claude/skills
                  # and the plugin cache) alongside the hotl skills dir.
# [skills.marketplaces]       # extra skill sources: name = git URL or local path
# acme = \"https://github.com/acme/skills.git\"   # managed: hotl skills add/update
# team = \"~/work/team-skills\"                   # local path: read in place

[network]
# Egress for bash commands: \"open\" (default), \"off\" (loopback + unix sockets
# only), or \"allowlist\" (loopback + the hosts below, via a local proxy).
# egress = \"open\"
# allow = [\"github.com\", \"*.crates.io\"]

[retention]
# Prune old sessions/shadows/blobs (run `hotl gc`, or auto at startup once set).
# max_age_days = 30
# max_sessions = 200

[history]
# Console prompt recall (Up/Down, Ctrl-R search), persisted across sessions.
# enabled = true                # false: recall works in-session, nothing on disk
# path = \"...\"                   # default: <xdg-data>/hotl/history.jsonl (~ expanded)
# max_entries = 1000            # oldest entries trimmed past this
# max_bytes = 2097152           # ...and past this size (2 MiB); smaller cap wins

# --- allow-rules: auto-approve trusted tool calls (TRUST GRANTS, not scopes) ---
# Anything not matched still asks. Protected paths (ssh/creds/build.rs/git hooks)
# ALWAYS ask, rule or not. Read-only conveniences are safe to auto-allow:
[[allow]]
tool = \"bash\"
prefix = \"ls \"

[[allow]]
tool = \"bash\"
prefix = \"cat \"

[[allow]]
tool = \"bash\"
prefix = \"git status\"

[[allow]]
tool = \"bash\"
prefix = \"git diff\"

[[allow]]
tool = \"bash\"
prefix = \"git log\"

# build/test runs code — opt in deliberately:
# [[allow]]
# tool = \"bash\"
# prefix = \"cargo test\"
#
# [[allow]]
# tool = \"edit\"
# path_prefix = \"src/\"

# --- MCP tool servers (first use asks, shows the binary + hash) ---
# [[mcp]]
# name = \"docs\"
# command = \"/usr/local/bin/docs-mcp\"
# args = [\"--stdio\"]
# description = \"project documentation search\"

# --- post-edit diagnostics: run your check command after edits ---
# [diagnostics]
# rs = \"cargo check -q --message-format=short\"

# --- hooks: intercept tool calls (pre_tool: deny/rewrite; post_tool: replace) ---
# [[hook]]
# event = \"pre_tool\"
# command = \"/usr/local/bin/guard\"
";

/// `hotl setup [--force]`: write the default config.toml, never silently.
pub fn setup_main(config_dir: &Path, force: bool) -> i32 {
    if let Err(e) = std::fs::create_dir_all(config_dir) {
        eprintln!("hotl: could not create {}: {e}", config_dir.display());
        return 1;
    }
    let cfg = config_dir.join("config.toml");
    match write_if_absent(&cfg, DEFAULT_CONFIG, force) {
        Wrote::Created => println!("wrote {}", cfg.display()),
        Wrote::Exists => println!("kept existing {} (use --force to overwrite)", cfg.display()),
        Wrote::Failed(e) => {
            eprintln!("hotl: could not write {}: {e}", cfg.display());
            return 1;
        }
    }
    println!(
        "config dir: {}\nnext: set a provider in [provider] (or HOTL_MODEL + a key), \
         then `hotl doctor` to verify.",
        config_dir.display()
    );
    0
}

enum Wrote {
    Created,
    Exists,
    Failed(std::io::Error),
}

fn write_if_absent(path: &Path, content: &str, force: bool) -> Wrote {
    if path.exists() && !force {
        return Wrote::Exists;
    }
    match std::fs::write(path, content) {
        Ok(()) => Wrote::Created,
        Err(e) => Wrote::Failed(e),
    }
}

/// One-line first-run hint printed by bare `hotl` when no config exists.
/// Never writes anything — pointing, not doing.
pub fn first_run_hint(config_dir: &Path) -> Option<String> {
    if config_dir.exists() {
        return None;
    }
    Some(format!(
        "first run — no config at {}. `hotl setup` writes safe defaults; `hotl doctor` checks your setup.",
        config_dir.display()
    ))
}

/// Expand `@[path]` tokens in a prompt to the file's contents (zsh `@[file]`
/// capture, M1 residual). A missing/unreadable file leaves the token in place
/// with an inline note — never silently dropped, never a hard error. Each
/// captured file is wrapped so the model knows where the text came from.
pub fn expand_file_refs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("@[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find(']') else {
            // No closing bracket: emit the rest verbatim and stop.
            out.push_str(&rest[start..]);
            return out;
        };
        let path = &after[..end];
        match std::fs::read_to_string(path) {
            Ok(content) => {
                out.push_str(&format!("\n<file path=\"{path}\">\n{content}\n</file>\n"));
            }
            Err(e) => out.push_str(&format!("@[{path}] (could not read: {e})")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Compare two `x.y.z` versions: is `latest` newer than `current`?
/// Non-numeric/short versions compare by the parts that parse (missing = 0).
pub fn is_newer(current: &str, latest: &str) -> bool {
    parts(latest) > parts(current)
}

fn parts(v: &str) -> (u64, u64, u64) {
    let v = v.trim_start_matches('v');
    let mut it = v
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_writes_then_preserves() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("hotl");
        assert_eq!(setup_main(&cfg, false), 0);
        assert!(cfg.join("config.toml").exists());
        // Second run without --force keeps the (possibly edited) file.
        std::fs::write(cfg.join("config.toml"), "# edited\n").unwrap();
        setup_main(&cfg, false);
        assert_eq!(
            std::fs::read_to_string(cfg.join("config.toml")).unwrap(),
            "# edited\n"
        );
        // --force overwrites.
        setup_main(&cfg, true);
        assert!(std::fs::read_to_string(cfg.join("config.toml"))
            .unwrap()
            .contains("allow"));
    }

    #[test]
    fn first_run_hint_only_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(first_run_hint(&dir.path().join("nope")).is_some());
        assert!(first_run_hint(dir.path()).is_none());
    }

    #[test]
    fn file_refs_expand_and_degrade() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("note.txt");
        std::fs::write(&f, "hello from file").unwrap();
        let prompt = format!("summarize @[{}] please", f.display());
        let out = expand_file_refs(&prompt);
        assert!(out.contains("hello from file") && out.contains("<file path="));
        assert!(out.starts_with("summarize") && out.trim_end().ends_with("please"));
        // A missing file leaves an inline note, not an error.
        let missing = expand_file_refs("see @[/no/such/file]");
        assert!(missing.contains("could not read"));
        // Text with no token is untouched.
        assert_eq!(expand_file_refs("plain prompt"), "plain prompt");
        // Unclosed token is left verbatim.
        assert_eq!(expand_file_refs("oops @[unclosed"), "oops @[unclosed");
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.1.2", "0.2.0"));
        assert!(is_newer("0.1.2", "0.1.3"));
        assert!(is_newer("v0.1.2", "v1.0.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.2", "0.1.2"));
    }
}
