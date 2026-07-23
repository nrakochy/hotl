//! `hotl skills` — inspect and register skill marketplaces.
//!
//! `list` shows every discovered skill with its source (`hotl`,
//! `<marketplace>`, `claude`, `claude:<plugin>`) plus a warning per
//! registered git marketplace whose checkout is missing. Registration
//! management (`add` / `update` / `remove`) lives here too; git runs only
//! on those explicit commands — discovery never touches the network.

use std::path::Path;

pub fn skills_main(args: &[String]) -> i32 {
    let config_dir = crate::agent::config_dir();
    match args.get(1).map(String::as_str) {
        None | Some("list") => {
            print!("{}", render_list(&config_dir));
            0
        }
        Some("add") => match (args.get(2), args.get(3)) {
            (Some(name), Some(source)) => report(add(&config_dir, name, source)),
            _ => usage(),
        },
        Some("update") => report(update(&config_dir, args.get(2).map(String::as_str))),
        Some("remove") => match args.get(2) {
            Some(name) => report(remove(&config_dir, name)),
            None => usage(),
        },
        _ => usage(),
    }
}

fn report(result: Result<String, String>) -> i32 {
    match result {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("hotl skills: {e}");
            1
        }
    }
}

/// Register a marketplace: validate the name, clone first when the source
/// is a git URL (config is written only after a successful clone), then
/// write the entry into config.toml preserving the document's text.
fn add(config_dir: &Path, name: &str, source: &str) -> Result<String, String> {
    let name = hotl_tools::skills::normalize_marketplace_name(name).ok_or_else(|| {
        format!(
            "`{name}` is not a valid marketplace name (letters, digits, \
             `.`/`_`/`-`, alphanumeric first char, ≤ 64 chars)"
        )
    })?;
    let path = config_dir.join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("config.toml is not valid TOML: {e}"))?;
    let registered = doc
        .get("skills")
        .and_then(|s| s.get("marketplaces"))
        .and_then(|m| m.get(&name))
        .is_some();
    if registered {
        return Err(format!("marketplace `{name}` is already registered"));
    }
    let mut note = String::new();
    if crate::config::is_git_url(source) {
        let dest = config_dir.join("marketplaces").join(&name);
        if dest.exists() {
            return Err(format!("{} already exists", dest.display()));
        }
        std::fs::create_dir_all(config_dir.join("marketplaces"))
            .map_err(|e| format!("cannot create marketplaces dir: {e}"))?;
        git(&["clone", source, &dest.to_string_lossy()])?;
        note = format!(" (cloned to {})", dest.display());
    }
    let skills = doc.entry("skills").or_insert(toml_edit::table());
    let skills = skills
        .as_table_mut()
        .ok_or("[skills] in config.toml is not a table")?;
    skills.set_implicit(true);
    let mkts = skills.entry("marketplaces").or_insert(toml_edit::table());
    let mkts = mkts
        .as_table_mut()
        .ok_or("[skills.marketplaces] in config.toml is not a table")?;
    mkts.insert(&name, toml_edit::value(source));
    std::fs::create_dir_all(config_dir).map_err(|e| format!("cannot create config dir: {e}"))?;
    std::fs::write(&path, doc.to_string()).map_err(|e| format!("cannot write config.toml: {e}"))?;
    Ok(format!("registered marketplace `{name}`{note}"))
}

/// `git pull --ff-only` each managed checkout (or just `only`); a
/// registered-but-missing checkout is cloned; local-path sources are
/// skipped with a note.
fn update(config_dir: &Path, only: Option<&str>) -> Result<String, String> {
    let cfg = crate::config::Config::load(config_dir);
    let mut lines = Vec::new();
    let mut matched = false;
    for (name, source) in &cfg.skills.marketplaces {
        if only.is_some_and(|o| o != name) {
            continue;
        }
        matched = true;
        if !crate::config::is_git_url(source) {
            lines.push(format!("{name}: local path — skipped"));
            continue;
        }
        let dest = config_dir.join("marketplaces").join(name);
        if dest.is_dir() {
            git(&["-C", &dest.to_string_lossy(), "pull", "--ff-only"])?;
            lines.push(format!("{name}: updated"));
        } else {
            std::fs::create_dir_all(config_dir.join("marketplaces"))
                .map_err(|e| format!("cannot create marketplaces dir: {e}"))?;
            git(&["clone", source, &dest.to_string_lossy()])?;
            lines.push(format!("{name}: cloned"));
        }
    }
    match (matched, only) {
        (false, Some(o)) => Err(format!("no marketplace named `{o}` is registered")),
        (false, None) => Ok("no marketplaces registered".into()),
        _ => Ok(lines.join("\n")),
    }
}

/// Unregister a marketplace. A managed checkout under
/// `<config_dir>/marketplaces/` is deleted (it is re-fetchable); a
/// local-path source is never touched.
fn remove(config_dir: &Path, name: &str) -> Result<String, String> {
    let path = config_dir.join("config.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| "no config.toml — nothing is registered".to_string())?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("config.toml is not valid TOML: {e}"))?;
    let source = doc
        .get("skills")
        .and_then(|s| s.get("marketplaces"))
        .and_then(|m| m.get(name))
        .and_then(|v| v.as_str())
        .map(String::from);
    let Some(source) = source else {
        return Err(format!("no marketplace named `{name}` is registered"));
    };
    doc["skills"]["marketplaces"]
        .as_table_mut()
        .ok_or("[skills.marketplaces] in config.toml is not a table")?
        .remove(name);
    std::fs::write(&path, doc.to_string()).map_err(|e| format!("cannot write config.toml: {e}"))?;
    let mut note = String::new();
    if crate::config::is_git_url(&source) {
        let dest = config_dir.join("marketplaces").join(name);
        if dest.is_dir() {
            std::fs::remove_dir_all(&dest).map_err(|e| format!("checkout not removed: {e}"))?;
            note = format!(" (checkout {} deleted)", dest.display());
        }
    }
    Ok(format!("removed marketplace `{name}`{note}"))
}

/// Run git with output passing through; actionable error when git itself
/// is missing from PATH.
fn git(args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new("git")
        .args(args)
        .status()
        .map_err(|e| format!("cannot run git ({e}) — is git on your PATH?"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`git {}` failed (see output above)",
            args.join(" ")
        ))
    }
}

fn usage() -> i32 {
    eprintln!(
        "usage: hotl skills [list] | add <name> <git-url|path> | update [name] | remove <name>"
    );
    2
}

/// The roster with a source column, then config warnings, then a warning
/// per registered git marketplace whose managed checkout is missing.
fn render_list(config_dir: &Path) -> String {
    let cfg = crate::config::Config::load(config_dir);
    let include_claude = cfg.skills.claude.unwrap_or(true);
    let (roots, warnings) = cfg.skills.marketplace_roots(config_dir);
    let tool = hotl_tools::skills::SkillTool::new(config_dir, include_claude, &roots);
    let mut out = String::new();
    if let Some(tool) = &tool {
        let width = tool
            .roster()
            .map(|(n, _, _)| n.chars().count())
            .max()
            .unwrap_or(0);
        for (name, source, desc) in tool.roster() {
            out.push_str(&format!("{name:<width$}  {source:<12}  {}\n", clip(desc)));
        }
    }
    if out.is_empty() {
        out.push_str("no skills discovered\n");
    }
    for w in &warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    for (name, source) in &cfg.skills.marketplaces {
        if crate::config::is_git_url(source) && !config_dir.join("marketplaces").join(name).is_dir()
        {
            out.push_str(&format!(
                "warning: marketplace `{name}` is registered but not fetched — \
                 run: hotl skills update {name}\n"
            ));
        }
    }
    out
}

/// One-line description clip for the list (char-boundary safe).
fn clip(s: &str) -> String {
    if s.chars().count() <= 80 {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(80).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_list_shows_sources_and_missing_checkout_warning() {
        let dir = tempfile::tempdir().unwrap();
        let mkt = dir.path().join("mkt");
        std::fs::create_dir_all(mkt.join("release")).unwrap();
        std::fs::write(
            mkt.join("release/SKILL.md"),
            "---\nname: release\ndescription: cut a release\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "[skills]\nclaude = false\n\n[skills.marketplaces]\n\
                 acme = \"{}\"\nghost = \"https://example.com/ghost.git\"\n",
                mkt.display()
            ),
        )
        .unwrap();
        let out = render_list(dir.path());
        assert!(out.contains("release") && out.contains("acme"), "{out}");
        assert!(
            out.contains("`ghost` is registered but not fetched"),
            "{out}"
        );
        assert!(out.contains("hotl skills update ghost"), "{out}");
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        let args: Vec<String> = vec!["skills".into(), "frobnicate".into()];
        assert_eq!(skills_main(&args), 2);
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} in {}", dir.display());
    }

    /// A local origin repo at a path ending in `.git`, so `add` treats it
    /// as a managed (cloned) source.
    fn make_origin(root: &Path) -> std::path::PathBuf {
        let origin = root.join("origin.git");
        std::fs::create_dir_all(origin.join("fixture")).unwrap();
        std::fs::write(
            origin.join("fixture/SKILL.md"),
            "---\nname: fixture\ndescription: fixture skill\n---\nbody\n",
        )
        .unwrap();
        git_in(&origin, &["init"]);
        git_in(&origin, &["add", "."]);
        git_in(&origin, &["commit", "-m", "init", "--no-gpg-sign"]);
        origin
    }

    #[test]
    fn add_local_path_writes_config_without_cloning() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "team", "/abs/team-skills").unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("[skills.marketplaces]"), "{text}");
        assert!(text.contains("team = \"/abs/team-skills\""), "{text}");
        assert!(!dir.path().join("marketplaces").exists());
        // Duplicate registration and invalid names error.
        assert!(add(dir.path(), "team", "/elsewhere").is_err());
        assert!(add(dir.path(), "bad:name", "/x").is_err());
    }

    #[test]
    fn add_preserves_existing_config_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "# my config\n[skills]\nclaude = false   # keep\n",
        )
        .unwrap();
        add(dir.path(), "team", "/abs/x").unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(
            text.contains("# my config") && text.contains("claude = false   # keep"),
            "{text}"
        );
        assert!(text.contains("team = \"/abs/x\""), "{text}");
    }

    #[test]
    fn remove_never_touches_a_local_path_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("team-skills");
        std::fs::create_dir_all(&src).unwrap();
        add(dir.path(), "team", &src.to_string_lossy()).unwrap();
        remove(dir.path(), "team").unwrap();
        assert!(src.is_dir());
        assert!(remove(dir.path(), "team").is_err());
    }

    #[test]
    fn add_update_remove_manage_a_git_checkout() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let origin = make_origin(dir.path());
        let source = origin.to_string_lossy().to_string();

        add(dir.path(), "acme", &source).unwrap();
        let checkout = dir.path().join("marketplaces/acme");
        assert!(checkout.join("fixture/SKILL.md").is_file());

        // A new skill lands in origin; update fast-forwards the checkout.
        std::fs::create_dir_all(origin.join("second")).unwrap();
        std::fs::write(
            origin.join("second/SKILL.md"),
            "---\nname: second\ndescription: second skill\n---\nbody\n",
        )
        .unwrap();
        git_in(&origin, &["add", "."]);
        git_in(&origin, &["commit", "-m", "second", "--no-gpg-sign"]);
        update(dir.path(), Some("acme")).unwrap();
        assert!(checkout.join("second/SKILL.md").is_file());

        // Unknown name errors; remove deletes the managed checkout.
        assert!(update(dir.path(), Some("nope")).is_err());
        remove(dir.path(), "acme").unwrap();
        assert!(!checkout.exists());
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(!text.contains("acme"), "{text}");
    }
}
