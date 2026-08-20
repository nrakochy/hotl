//! `hotl workflows` — list saved recipes, show one (as TOML or Mermaid), or
//! check a plan file before saving it. Discovery only reads
//! `<config_dir>/workflows/*.toml`; nothing here runs an agent.

use std::path::Path;

pub fn workflows_main(args: &[String]) -> i32 {
    let config_dir = crate::agent::config_dir();
    match args.get(1).map(String::as_str) {
        None | Some("list") => {
            print!("{}", render_list(&config_dir));
            0
        }
        Some("show") => match args.get(2) {
            Some(name) => {
                let mermaid = args.iter().any(|a| a == "--mermaid");
                report(show(&config_dir, name, mermaid))
            }
            None => usage(),
        },
        Some("check") => match args.get(2) {
            Some(file) => report(check(Path::new(file))),
            None => usage(),
        },
        _ => usage(),
    }
}

fn report(result: Result<String, String>) -> i32 {
    match result {
        Ok(msg) => {
            print!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("hotl workflows: {e}");
            1
        }
    }
}

fn usage() -> i32 {
    eprintln!("usage: hotl workflows [list] | show <name> [--mermaid] | check <file>");
    2
}

/// Name, description and summary per recipe; unloadable files listed after.
fn render_list(config_dir: &Path) -> String {
    let found = hotl_workflow::discover(config_dir);
    if found.is_empty() {
        return format!(
            "no saved workflows — add *.toml files under {}\n",
            hotl_workflow::discover::root(config_dir).display()
        );
    }
    let mut out = String::new();
    let mut broken = Vec::new();
    for f in found {
        match f.plan {
            Ok(plan) => {
                out.push_str(&format!(
                    "{:<20} {}\n{:<20} {}\n",
                    f.name,
                    plan.description.as_deref().unwrap_or(""),
                    "",
                    plan.summary_line(0)
                ));
            }
            Err(e) => broken.push(e),
        }
    }
    for e in broken {
        out.push_str(&format!("warning: {e}\n"));
    }
    out
}

/// The recipe's TOML verbatim, or its Mermaid rendering.
fn show(config_dir: &Path, name: &str, mermaid: bool) -> Result<String, String> {
    let path = hotl_workflow::discover::root(config_dir).join(format!("{name}.toml"));
    let Some(loaded) = hotl_workflow::discover::load(config_dir, name) else {
        let names: Vec<String> = hotl_workflow::discover::list(config_dir)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        return Err(format!(
            "no saved workflow `{name}` (have: {})",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        ));
    };
    let plan = loaded?;
    if mermaid {
        return Ok(hotl_workflow::mermaid::render(&plan));
    }
    std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Validate a plan file: the approval summary and any warnings on success,
/// every error on failure.
fn check(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let plan =
        hotl_workflow::Plan::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let findings = plan.validate();
    let errors: Vec<String> = findings
        .iter()
        .filter(|f| f.severity == hotl_workflow::Severity::Error)
        .map(ToString::to_string)
        .collect();
    if !errors.is_empty() {
        return Err(format!(
            "{} is not a valid plan:\n  {}",
            path.display(),
            errors.join("\n  ")
        ));
    }
    let mut out = format!("{}\n", plan.summary_line(0));
    for w in findings {
        out.push_str(&format!("warning: {w}\n"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../hotl-workflow/tests/fixtures/review-changes.toml");

    fn config_with_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let wf = dir.path().join("workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("review-changes.toml"), FIXTURE).unwrap();
        std::fs::write(wf.join("broken.toml"), "name = \"broken\"\n").unwrap();
        dir
    }

    #[test]
    fn check_prints_the_summary_for_a_good_plan_and_every_error_for_a_bad_one() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.toml");
        std::fs::write(&good, FIXTURE).unwrap();
        let out = check(&good).unwrap();
        assert!(
            out.starts_with("workflow `review-changes` — 3 phases, ≈15+ agents: Review (2 ∥)"),
            "{out}"
        );

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "name = \"Bad Name\"\n[[phases]]\ntitle = \"A\"\nagents = []\n[[phases]]\ntitle = \"A\"\nagents = []\n").unwrap();
        let err = check(&bad).unwrap_err();
        assert!(err.contains("not a valid plan"), "{err}");
        assert!(
            err.contains("^[a-z0-9][a-z0-9-]*$")
                && err.contains("duplicate title")
                && err.contains("`agents` is empty"),
            "{err}"
        );
        assert!(check(Path::new("/nonexistent/x.toml")).is_err());
    }

    #[test]
    fn show_prints_toml_or_mermaid_and_names_the_available_recipes() {
        let dir = config_with_fixture();
        assert_eq!(show(dir.path(), "review-changes", false).unwrap(), FIXTURE);
        let m = show(dir.path(), "review-changes", true).unwrap();
        assert!(
            m.starts_with("flowchart LR\n") && m.contains("Find --> Find"),
            "{m}"
        );
        let e = show(dir.path(), "nope", false).unwrap_err();
        assert!(
            e.contains("no saved workflow `nope`") && e.contains("review-changes"),
            "{e}"
        );
        assert!(show(dir.path(), "broken", false)
            .unwrap_err()
            .contains("at least one phase"));
    }

    #[test]
    fn list_shows_recipes_then_warnings() {
        let dir = config_with_fixture();
        let out = render_list(dir.path());
        assert!(
            out.starts_with("review-changes       Review the diff across dimensions"),
            "{out}"
        );
        assert!(
            out.contains("warning:") && out.contains("broken.toml"),
            "{out}"
        );
        let empty = tempfile::tempdir().unwrap();
        assert!(render_list(empty.path()).starts_with("no saved workflows"));
    }
}
