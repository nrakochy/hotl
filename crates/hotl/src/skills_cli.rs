//! `hotl skills` — inspect and register skill marketplaces.
//!
//! `list` shows every discovered skill with its source (`hotl`,
//! `<marketplace>`, `claude`, `claude:<plugin>`) plus a warning per
//! registered git marketplace whose checkout is missing. Registration
//! management (`add` / `update` / `remove`) lives here too; git runs only
//! on those explicit commands — discovery never touches the network.

use std::path::Path;

pub fn skills_main(args: &[String]) -> i32 {
    match args.get(1).map(String::as_str) {
        None | Some("list") => {
            print!("{}", render_list(&crate::agent::config_dir()));
            0
        }
        // `add` / `update` / `remove` land with the management task.
        Some("add") | Some("update") | Some("remove") => usage(),
        _ => usage(),
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
    let width = tool
        .roster()
        .map(|(n, _, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (name, source, desc) in tool.roster() {
        out.push_str(&format!("{name:<width$}  {source:<12}  {}\n", clip(desc)));
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
}
