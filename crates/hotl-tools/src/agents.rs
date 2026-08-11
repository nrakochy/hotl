//! User-defined subagent shapes (tier-1 gap #6): data-driven agent types
//! instead of one hardcoded `spawn` shape.
//!
//! An [`AgentDef`] selects a system prompt, a tool subset ([`ToolScope`]), a
//! model/effort, and (implicitly, via the tool subset) a capability posture.
//! Three built-in types (`general-purpose`, `explore`, `plan`) ship as Rust
//! consts and can never be shadowed by a user definition — `resolve` skips a
//! same-named user file with a warning rather than silently overriding it.
//! Beyond the built-ins, agent defs load from `agents/*.md` under the hotl
//! config dir and (Claude-compat, opt-out) `~/.claude/agents/*.md`, parsed
//! frontmatter-only — the same progressive-disclosure shape as `skills.rs`
//! (the tool description names what exists; content loads only when used).
//!
//! **Depth-1 stays structural here too:** [`filter_registry`] builds every
//! child registry from scratch and defensively excludes `spawn` regardless of
//! the resolved [`ToolScope`] — a user agent def cannot re-enable recursion
//! by naming `spawn` in a `tools:` list.

use std::path::{Path, PathBuf};

use hotl_provider::effort::EFFORT_LEVELS;

/// Re-exported so surfaces that already reach this crate for `PermissionMode`
/// parse an effort through the *same* type the wire uses, rather than growing
/// a second spelling list.
pub use hotl_provider::Effort;

use crate::Registry;

/// Where an [`AgentDef`] came from — provenance only, not a trust signal by
/// itself (a user def's *system prompt* is still just instructions the owner
/// wrote; it is not adversarial the way subagent *output* is).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    BuiltIn,
    User,
    Claude,
}

/// Which tools a resolved agent def's child registry keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolScope {
    /// Every builtin (still never `spawn` — depth-1 is structural).
    All,
    /// Only tools whose `Tool::read_only()` is true.
    ReadOnly,
    /// Only the named builtins (unknown names simply match nothing).
    Only(Vec<String>),
}

/// Whether a child gets its own git worktree to work in.
///
/// A def's `isolation:` frontmatter wins; `[agents] isolation` in config.toml
/// is the fallback; off is the default. Read-only defs are never isolated
/// regardless — `explore` and `plan` have nothing to isolate and a checkout is
/// not free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    #[default]
    None,
    Worktree,
}

/// `"worktree"` | `"none"`. Fail-closed like `SandboxCfg::file_tools`: an
/// unknown value is `None` plus a warning, never a guess. Shared by the
/// frontmatter parse and the `[agents] isolation` config parse so the two can
/// never drift.
pub fn parse_isolation(raw: &str, whose: &str) -> Isolation {
    match raw.trim() {
        "worktree" => Isolation::Worktree,
        "none" | "" => Isolation::None,
        other => {
            eprintln!(
                "hotl: {whose} sets `isolation: {other}`, which is not a known value \
                 (`worktree` | `none`) — running without isolation."
            );
            Isolation::None
        }
    }
}

/// `low | medium | high | xhigh | max`. Warns and yields `None` on anything
/// else — the same posture as [`parse_isolation`]: a typo in a def must not
/// take the def down with it, and `ultra` (which this ladder deliberately does
/// not carry) is exactly the word someone will try.
pub fn parse_effort(raw: &str, whose: &str) -> Option<Effort> {
    match raw.trim().parse::<Effort>() {
        Ok(e) => Some(e),
        Err(_) => {
            eprintln!(
                "hotl: {whose} sets `effort: {}`, which is not a known level \
                 ({EFFORT_LEVELS}) — inheriting instead.",
                raw.trim()
            );
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// `None` = inherit the parent's system prompt (a `fork`'s default
    /// shape); `Some` = this def's own persona.
    pub system_prompt: Option<String>,
    pub tools: ToolScope,
    pub model: Option<String>,
    /// Reasoning depth for this child only, replacing the parent's. Validated
    /// here at parse time (see [`parse_effort`]) so
    /// `hotl::agent::HotlChildBuilder::spawn_child` has nothing left to reject
    /// — it runs inside the TUI process, where a warning would land on the
    /// alternate screen (T3-23).
    pub effort: Option<Effort>,
    /// Whether this def's children get their own git worktree. `None` here
    /// means "unset", not "off": the `[agents] isolation` default still
    /// applies (`hotl::agent::HotlChildBuilder::spawn_child`).
    pub isolation: Isolation,
    pub source: AgentSource,
}

/// The three built-in agent types, matching the corpus convergence (03):
/// `general-purpose` (full access), `explore`/`plan` (read-only).
pub const BUILTIN_NAMES: [&str; 3] = ["general-purpose", "explore", "plan"];

/// A built-in agent type by name, or `None` if `name` isn't one of the
/// three. Built-in names always win over a same-named user definition —
/// see [`resolve`].
pub fn builtin(name: &str) -> Option<AgentDef> {
    match name {
        "general-purpose" => Some(AgentDef {
            name: "general-purpose".into(),
            description: "Full-access sub-agent for open-ended, self-contained subtasks \
                (research, implement, summarize) — gets the base `read`/`edit`/`write`/`bash`/\
                `glob`/`grep` builtins, never the parent's `web_fetch`/`web_search`/MCP/skills/\
                `recall` extensions."
                .into(),
            system_prompt: None,
            tools: ToolScope::All,
            model: None,
            effort: None,
            // The three built-ins stay unset; `[agents] isolation` is what
            // turns worktrees on for `general-purpose`.
            isolation: Isolation::None,
            source: AgentSource::BuiltIn,
        }),
        "explore" => Some(AgentDef {
            name: "explore".into(),
            description: "Fast, read-only search: locate code, files, and answers. \
                Cannot write, edit, or run commands — safe to fan out in parallel."
                .into(),
            system_prompt: Some(
                "You are a fast, read-only exploration sub-agent. Use `read`/`glob`/`grep` to \
                 answer the question precisely and efficiently. You have no write, edit, or bash \
                 access — never claim to have made a change. Report findings concisely, citing \
                 file paths and line numbers where useful."
                    .into(),
            ),
            tools: ToolScope::ReadOnly,
            model: None,
            effort: None,
            isolation: Isolation::None,
            source: AgentSource::BuiltIn,
        }),
        "plan" => Some(AgentDef {
            name: "plan".into(),
            description: "Read-only planning: investigate, then propose a concrete step-by-step \
                plan without touching the workspace."
                .into(),
            system_prompt: Some(
                "You are a planning sub-agent. Investigate using only read-only tools \
                 (`read`/`glob`/`grep`), then propose a concrete, step-by-step implementation \
                 plan. You cannot edit files or run commands — describe what should change; do \
                 not do it yourself."
                    .into(),
            ),
            tools: ToolScope::ReadOnly,
            model: None,
            effort: None,
            isolation: Isolation::None,
            source: AgentSource::BuiltIn,
        }),
        _ => None,
    }
}

/// Parse one `agents/*.md` file: a `---`-fenced frontmatter block
/// (`name:`/`description:`/`tools:`/`model:`/`effort:`) followed by the
/// system prompt body. Returns `None` if there's no frontmatter fence or no
/// `name:` field — callers scanning a directory fall back to the filename
/// via [`parse_def_or_named`].
///
/// `isolation:` (`worktree` | `none`) selects whether this def's children get
/// their own git worktree; see [`Isolation`]. It is the highest-precedence
/// setting — `[agents] isolation` only applies to defs that stay silent.
pub fn parse_def(text: &str, source: AgentSource) -> Option<AgentDef> {
    parse_def_or_named(text, source, None)
}

/// Like [`parse_def`], but a missing `name:` field falls back to
/// `fallback_name` (the file's stem) instead of failing outright — the same
/// "frontmatter name, else the filename" shape `skills.rs::read_skill_dir`
/// uses.
pub fn parse_def_or_named(
    text: &str,
    source: AgentSource,
    fallback_name: Option<&str>,
) -> Option<AgentDef> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let fence = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n');
    let get = |key: &str| {
        fence
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
            .filter(|v| !v.is_empty())
    };
    let name = get("name:").or_else(|| fallback_name.map(str::to_string))?;
    let description = get("description:").unwrap_or_default();
    let tools = get("tools:")
        .map(|s| parse_tool_scope(&s))
        .unwrap_or(ToolScope::All);
    let model = get("model:");
    let effort = get("effort:").and_then(|raw| parse_effort(&raw, &format!("agent def `{name}`")));
    let isolation = get("isolation:")
        .map(|raw| parse_isolation(&raw, &format!("agent def `{name}`")))
        .unwrap_or_default();
    let system_prompt = if body.trim().is_empty() {
        None
    } else {
        Some(body.trim().to_string())
    };
    Some(AgentDef {
        name,
        description,
        system_prompt,
        tools,
        model,
        effort,
        isolation,
        source,
    })
}

/// `all` | `read-only`/`readonly` | a comma list of tool names.
fn parse_tool_scope(raw: &str) -> ToolScope {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("all") {
        return ToolScope::All;
    }
    if s.eq_ignore_ascii_case("read-only") || s.eq_ignore_ascii_case("readonly") {
        return ToolScope::ReadOnly;
    }
    ToolScope::Only(
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
    )
}

/// This process's `agents/*.md` root under the owner's config dir.
fn user_agents_root(config_dir: &Path) -> PathBuf {
    config_dir.join("agents")
}

/// `~/.claude/agents` (Claude-compat root; opt-out via `include_claude`).
fn claude_agents_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".claude/agents")
}

/// Every def in one `agents/*.md` root, name-sorted, deterministic. A file
/// whose frontmatter names a built-in is skipped with a warning — built-ins
/// can never be shadowed, silently or otherwise (03 "user definitions cannot
/// shadow built-ins").
fn scan_root(dir: &Path, source: AgentSource) -> Vec<AgentDef> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stem = path.file_stem().and_then(|s| s.to_str());
        let Some(def) = parse_def_or_named(&text, source, stem) else {
            continue;
        };
        if builtin(&def.name).is_some() {
            eprintln!(
                "hotl: agent def `{}` in {} shadows a built-in agent type and is ignored \
                 — built-in names cannot be overridden.",
                def.name,
                path.display()
            );
            continue;
        }
        out.push(def);
    }
    out
}

/// Resolve one agent type by name. Precedence: built-in name always wins;
/// else the owner's `agents/*.md`; else (if `include_claude`)
/// `~/.claude/agents/*.md`. `None` if nothing matches.
pub fn resolve(config_dir: &Path, include_claude: bool, name: &str) -> Option<AgentDef> {
    if let Some(b) = builtin(name) {
        return Some(b);
    }
    if let Some(d) = scan_root(&user_agents_root(config_dir), AgentSource::User)
        .into_iter()
        .find(|d| d.name == name)
    {
        return Some(d);
    }
    if include_claude {
        if let Some(d) = scan_root(&claude_agents_root(), AgentSource::Claude)
            .into_iter()
            .find(|d| d.name == name)
        {
            return Some(d);
        }
    }
    None
}

/// `(name, description)` for every available agent type — built-ins first,
/// then user, then (if enabled) Claude — for the `spawn` tool's error/
/// description text.
pub fn list(config_dir: &Path, include_claude: bool) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = BUILTIN_NAMES
        .iter()
        .map(|n| {
            let b = builtin(n).expect("BUILTIN_NAMES only lists real builtins");
            (b.name, b.description)
        })
        .collect();
    out.extend(
        scan_root(&user_agents_root(config_dir), AgentSource::User)
            .into_iter()
            .map(|d| (d.name, d.description)),
    );
    if include_claude {
        out.extend(
            scan_root(&claude_agents_root(), AgentSource::Claude)
                .into_iter()
                .map(|d| (d.name, d.description)),
        );
    }
    out
}

/// Build a child's registry from `full` per the resolved def's [`ToolScope`].
/// `spawn` is defensively excluded regardless of scope — depth-1 is a
/// structural invariant, not something a `tools:` list can override.
pub fn filter_registry(def: &AgentDef, full: &Registry) -> Registry {
    match &def.tools {
        ToolScope::All => full.filtered(|_| true),
        ToolScope::ReadOnly => full.filtered(|t| t.read_only()),
        ToolScope::Only(names) => full.filtered(|t| names.iter().any(|n| n == t.name())),
    }
}

/// Whether a resolved def's *filtered* toolset is entirely read-only — the
/// concurrency guard's read on "can this child run in parallel with another
/// mutating child" (index: "guard parallel mutating children"). Computed
/// dynamically off the actual filtered set (not just `ToolScope::ReadOnly`)
/// so an `Only([...])` list of exclusively read tools also counts.
pub fn is_read_only(def: &AgentDef) -> bool {
    filter_registry(def, &Registry::builtin()).all_read_only()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_present_and_read_only_explore() {
        assert_eq!(builtin("explore").unwrap().tools, ToolScope::ReadOnly);
        assert_eq!(builtin("plan").unwrap().tools, ToolScope::ReadOnly);
        assert_eq!(builtin("general-purpose").unwrap().tools, ToolScope::All);
        assert!(builtin("nope").is_none());
    }

    #[test]
    fn parse_def_reads_frontmatter_and_body() {
        let md = "---\nname: reviewer\ndescription: reviews diffs\ntools: read-only\nmodel: claude-haiku-4-5-20251001\n---\nYou are a strict reviewer.\n";
        let d = parse_def(md, AgentSource::User).unwrap();
        assert_eq!(d.name, "reviewer");
        assert_eq!(d.tools, ToolScope::ReadOnly);
        assert_eq!(d.model.as_deref(), Some("claude-haiku-4-5-20251001"));
        assert!(d.system_prompt.unwrap().contains("strict reviewer"));
    }

    #[test]
    fn parse_def_tool_list() {
        let md = "---\nname: builder\ntools: read, write, edit, bash\n---\nbody";
        let d = parse_def(md, AgentSource::User).unwrap();
        assert_eq!(
            d.tools,
            ToolScope::Only(vec![
                "read".into(),
                "write".into(),
                "edit".into(),
                "bash".into()
            ])
        );
    }

    #[test]
    fn parse_def_without_name_needs_a_fallback() {
        let md = "---\ndescription: no name here\n---\nbody";
        assert!(parse_def(md, AgentSource::User).is_none());
        let d = parse_def_or_named(md, AgentSource::User, Some("fallback")).unwrap();
        assert_eq!(d.name, "fallback");
    }

    #[test]
    fn resolve_prefers_builtin_and_ignores_shadowing_file() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        // Tries to override the builtin `plan` — must lose to the builtin.
        std::fs::write(
            agents.join("plan.md"),
            "---\nname: plan\ntools: all\n---\nEvil override\n",
        )
        .unwrap();
        std::fs::write(
            agents.join("reviewer.md"),
            "---\nname: reviewer\ndescription: reviews diffs\ntools: read-only\n---\nReview carefully.\n",
        )
        .unwrap();

        let plan = resolve(dir.path(), false, "plan").unwrap();
        assert_eq!(plan.tools, ToolScope::ReadOnly, "builtin plan must win");
        assert_eq!(plan.source, AgentSource::BuiltIn);

        let reviewer = resolve(dir.path(), false, "reviewer").unwrap();
        assert_eq!(reviewer.source, AgentSource::User);
        assert_eq!(reviewer.tools, ToolScope::ReadOnly);

        assert!(resolve(dir.path(), false, "nope").is_none());
    }

    #[test]
    fn list_includes_builtins_and_user_defs() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("reviewer.md"),
            "---\nname: reviewer\ndescription: reviews diffs\n---\nbody",
        )
        .unwrap();
        let names: Vec<String> = list(dir.path(), false)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert!(names.contains(&"general-purpose".to_string()));
        assert!(names.contains(&"explore".to_string()));
        assert!(names.contains(&"plan".to_string()));
        assert!(names.contains(&"reviewer".to_string()));
    }

    #[test]
    fn filter_registry_read_only_keeps_only_reads() {
        let def = builtin("explore").unwrap();
        let child = filter_registry(&def, &Registry::builtin());
        assert!(child.get("read").is_some() && child.get("glob").is_some());
        assert!(child.get("write").is_none() && child.get("bash").is_none());
        assert!(child.get("spawn").is_none(), "children never recurse");
    }

    #[test]
    fn filter_registry_all_still_excludes_spawn_structurally() {
        // Depth-1 is structural: even a registry that (hypothetically)
        // contains `spawn` must never survive `filter_registry`, regardless
        // of `ToolScope::All`. This is the depth-1 invariant assertion the
        // plan calls for.
        struct FakeSpawn;
        impl crate::Tool for FakeSpawn {
            fn name(&self) -> &'static str {
                "spawn"
            }
            fn description(&self) -> &str {
                "fake"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn permission(&self, _input: &serde_json::Value) -> crate::Permission {
                crate::Permission::None
            }
            fn run<'a>(
                &'a self,
                _input: serde_json::Value,
                _cancel: tokio_util::sync::CancellationToken,
            ) -> futures_util::future::BoxFuture<'a, crate::ToolOutcome> {
                Box::pin(async { crate::ToolOutcome::ok("") })
            }
        }
        let mut full = Registry::builtin();
        full.register(Box::new(FakeSpawn));
        assert!(full.get("spawn").is_some(), "test setup sanity check");

        let all_def = AgentDef {
            name: "x".into(),
            description: String::new(),
            system_prompt: None,
            tools: ToolScope::All,
            model: None,
            effort: None,
            isolation: Isolation::None,
            source: AgentSource::User,
        };
        let child = filter_registry(&all_def, &full);
        assert!(
            child.get("spawn").is_none(),
            "spawn must never survive filter_registry, even under ToolScope::All"
        );

        let only_def = AgentDef {
            tools: ToolScope::Only(vec!["spawn".into(), "read".into()]),
            ..all_def
        };
        let child = filter_registry(&only_def, &full);
        assert!(
            child.get("spawn").is_none(),
            "spawn must never survive filter_registry, even if named explicitly in `tools:`"
        );
        assert!(child.get("read").is_some());
    }

    #[test]
    fn is_read_only_reflects_the_filtered_set() {
        assert!(is_read_only(&builtin("explore").unwrap()));
        assert!(!is_read_only(&builtin("general-purpose").unwrap()));
        let only_reads = AgentDef {
            name: "x".into(),
            description: String::new(),
            system_prompt: None,
            tools: ToolScope::Only(vec!["read".into(), "glob".into()]),
            model: None,
            effort: None,
            isolation: Isolation::None,
            source: AgentSource::User,
        };
        assert!(is_read_only(&only_reads));
        let mixed = AgentDef {
            tools: ToolScope::Only(vec!["read".into(), "bash".into()]),
            ..only_reads
        };
        assert!(!is_read_only(&mixed));
    }

    #[test]
    fn parse_def_stores_worktree_isolation() {
        let md = "---\nname: worktreed\nisolation: worktree\n---\nbody";
        let d = parse_def(md, AgentSource::User).unwrap();
        assert_eq!(d.name, "worktreed");
        assert_eq!(d.tools, ToolScope::All);
        assert_eq!(d.isolation, Isolation::Worktree);

        // Silent and explicit-`none` defs are both unset.
        let silent = parse_def("---\nname: plain\n---\nbody", AgentSource::User).unwrap();
        assert_eq!(silent.isolation, Isolation::None);
        let off = parse_def(
            "---\nname: p\nisolation: none\n---\nbody",
            AgentSource::User,
        )
        .unwrap();
        assert_eq!(off.isolation, Isolation::None);
    }

    /// Fail-closed, the same shape `SandboxCfg::file_tools` uses: a typo does
    /// not silently become the permissive reading.
    #[test]
    fn parse_def_falls_back_to_none_on_an_unknown_isolation_value() {
        let md = "---\nname: typo\nisolation: worktee\n---\nbody";
        let d = parse_def(md, AgentSource::User).unwrap();
        assert_eq!(d.isolation, Isolation::None);
        assert_eq!(parse_isolation("container", "test"), Isolation::None);
    }

    #[test]
    fn parse_def_stores_the_effort_ladder_rung() {
        let d = parse_def(
            "---\nname: deep\neffort: xhigh\n---\nbody",
            AgentSource::User,
        )
        .unwrap();
        assert_eq!(d.effort, Some(Effort::XHigh));
        // The aliases the ladder tolerates reach the def too.
        let alias = parse_def(
            "---\nname: deep\neffort: x-high\n---\nbody",
            AgentSource::User,
        )
        .unwrap();
        assert_eq!(alias.effort, Some(Effort::XHigh));
        let silent = parse_def("---\nname: plain\n---\nbody", AgentSource::User).unwrap();
        assert_eq!(silent.effort, None);
    }

    /// `ultra` is the word someone arriving from another harness will try, and
    /// the rung this ladder deliberately does not carry. It warns and the def
    /// still loads — a typo must not cost the whole agent.
    #[test]
    fn an_unknown_effort_drops_to_none_and_the_def_still_loads() {
        let d = parse_def(
            "---\nname: typo\neffort: ultra\n---\nbody",
            AgentSource::User,
        )
        .unwrap();
        assert_eq!(d.name, "typo");
        assert_eq!(d.effort, None);
        assert_eq!(parse_effort("ultra", "test"), None);
    }
}
