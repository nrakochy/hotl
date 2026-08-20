//! Saved recipes: `<config_dir>/workflows/*.toml`, regular files only, no
//! descent, and the file stem must equal the plan's `name` — so `/name` and
//! `hotl workflows show <name>` can never disagree about which file they mean.

use std::path::{Path, PathBuf};

use crate::plan::Plan;

#[derive(Debug, Clone)]
pub struct Found {
    /// The file stem — the name the recipe is invoked by.
    pub name: String,
    pub path: PathBuf,
    /// `Err` is the message the caller surfaces as a warning.
    pub plan: Result<Plan, String>,
}

pub fn root(config_dir: &Path) -> PathBuf {
    config_dir.join("workflows")
}

/// Every `*.toml` in the root, stem-sorted. Parse and validation failures
/// come back as `Err` entries rather than being dropped.
pub fn discover(config_dir: &Path) -> Vec<Found> {
    let dir = root(config_dir);
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_string();
            let plan = load_file(&path).and_then(|plan| {
                if plan.name != name {
                    return Err(format!(
                        "{}: the plan is named `{}` but the file is `{name}.toml` — rename one to match",
                        path.display(),
                        plan.name
                    ));
                }
                Ok(plan)
            });
            Some(Found { name, path, plan })
        })
        .collect()
}

/// Read + parse + validate one file.
pub fn load_file(path: &Path) -> Result<Plan, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let plan = Plan::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let errors = plan.errors();
    if !errors.is_empty() {
        return Err(format!(
            "{}:\n{}",
            path.display(),
            errors
                .iter()
                .map(|e| format!("  {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    Ok(plan)
}

/// One saved recipe by name; `None` when no such file exists.
pub fn load(config_dir: &Path, name: &str) -> Option<Result<Plan, String>> {
    let path = root(config_dir).join(format!("{name}.toml"));
    path.is_file().then(|| {
        load_file(&path).and_then(|plan| {
            if plan.name != name {
                return Err(format!(
                    "{}: the plan is named `{}`, not `{name}`",
                    path.display(),
                    plan.name
                ));
            }
            Ok(plan)
        })
    })
}

/// `(name, description)` of every loadable recipe — the `/` completion roster.
pub fn list(config_dir: &Path) -> Vec<(String, String)> {
    discover(config_dir)
        .into_iter()
        .filter_map(|f| {
            let plan = f.plan.ok()?;
            Some((f.name, plan.description.unwrap_or_default()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::FIXTURE;

    #[test]
    fn reads_only_toml_files_at_the_top_level_and_checks_stem_against_name() {
        let dir = tempfile::tempdir().unwrap();
        let wf = root(dir.path());
        std::fs::create_dir_all(wf.join("nested")).unwrap();
        std::fs::write(wf.join("review-changes.toml"), FIXTURE).unwrap();
        std::fs::write(
            wf.join("mismatch.toml"),
            FIXTURE.replacen("review-changes", "other-name", 1),
        )
        .unwrap();
        std::fs::write(wf.join("broken.toml"), "name = \"broken\"\nphases = 3\n").unwrap();
        std::fs::write(wf.join("invalid.toml"), "name = \"invalid\"\n").unwrap();
        std::fs::write(wf.join("notes.md"), "# not a recipe").unwrap();
        std::fs::write(wf.join("nested").join("deep.toml"), FIXTURE).unwrap();

        let found = discover(dir.path());
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["broken", "invalid", "mismatch", "review-changes"]);
        assert!(found[0].plan.as_ref().unwrap_err().contains("broken.toml"));
        assert!(found[1]
            .plan
            .as_ref()
            .unwrap_err()
            .contains("at least one phase"));
        let e = found[2].plan.as_ref().unwrap_err();
        assert!(
            e.contains("named `other-name`") && e.contains("mismatch.toml"),
            "{e}"
        );
        assert!(found[3].plan.is_ok());

        assert_eq!(
            list(dir.path()),
            vec![(
                "review-changes".to_string(),
                "Review the diff across dimensions, then verify each finding".to_string()
            )]
        );
        assert!(load(dir.path(), "review-changes").unwrap().is_ok());
        assert!(load(dir.path(), "mismatch").unwrap().is_err());
        assert!(load(dir.path(), "deep").is_none(), "no descent");
        assert!(load(dir.path(), "nope").is_none());
    }

    #[test]
    fn a_missing_root_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover(dir.path()).is_empty());
        assert!(list(dir.path()).is_empty());
    }
}
