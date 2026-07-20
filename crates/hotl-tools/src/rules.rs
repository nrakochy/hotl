//! Allow-rule persistence (0001 §M1; unlocked by the sandbox floor — r2 R3).
//!
//! Deliberately file-only: rules live in `~/.config/hotl/permissions.toml`
//! and are written by the human with an editor, never by an in-REPL "always
//! allow" reflex — ask-fatigue was the attack the round-2 review flagged, so
//! persistence is an act of deliberate configuration, not a keystroke.
//!
//! Evaluation is deny-first with two hard carve-outs:
//! 1. **Protected execute-later paths never auto-allow**, no matter what a
//!    rule says.
//! 2. **Bash rules only apply while the kernel sandbox floor is enforced** —
//!    on an unsandboxed host every bash call still asks.
//!
//! ```toml
//! # ~/.config/hotl/permissions.toml
//! [[allow]]
//! tool = "bash"
//! prefix = "cargo "        # command prefix
//!
//! [[allow]]
//! tool = "write"           # or "edit"
//! path_prefix = "src/"
//! ```

use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
pub struct Rules {
    #[serde(default)]
    allow: Vec<AllowRule>,
}

#[derive(Debug, Deserialize)]
pub struct AllowRule {
    tool: String,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    path_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A rule matched; skip the ask (the surface still narrates it).
    Auto { rule: String },
    /// No rule (or a carve-out applies): ask the human.
    Ask,
}

impl Rules {
    /// Load rules; a malformed file is ignored and reported back to the
    /// caller (libraries don't print — the surface decides how to warn).
    pub fn load(config_dir: &Path) -> (Self, Option<String>) {
        let path = config_dir.join("permissions.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Rules>(&text) {
                Ok(rules) => (rules, None),
                Err(e) => (Rules::default(), Some(format!("ignoring malformed {}: {e}", path.display()))),
            },
            Err(_) => (Rules::default(), None),
        }
    }

    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty()
    }

    /// Deny-first evaluation. `protected` is Some when the target is in the
    /// execute-later class; `sandbox_enforced` reflects the live floor.
    pub fn evaluate(
        &self,
        tool: &str,
        input: &Value,
        sandbox_enforced: bool,
        protected: bool,
    ) -> Verdict {
        if protected {
            return Verdict::Ask; // carve-out 1: never auto into execute-later paths
        }
        for rule in &self.allow {
            if rule.tool != tool {
                continue;
            }
            match tool {
                "bash" => {
                    if !sandbox_enforced {
                        continue; // carve-out 2: bash rules need the floor
                    }
                    let Some(prefix) = &rule.prefix else { continue };
                    let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
                    if cmd.starts_with(prefix.as_str()) {
                        return Verdict::Auto { rule: format!("bash prefix `{prefix}`") };
                    }
                }
                "write" | "edit" => {
                    let Some(pp) = &rule.path_prefix else { continue };
                    let path = input.get("path").and_then(Value::as_str).unwrap_or("");
                    let normalized = path.trim_start_matches("./");
                    if normalized.starts_with(pp.as_str()) {
                        return Verdict::Auto { rule: format!("{tool} path `{pp}`") };
                    }
                }
                _ => {}
            }
        }
        Verdict::Ask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules() -> Rules {
        Rules::from_toml(
            r#"
[[allow]]
tool = "bash"
prefix = "cargo "

[[allow]]
tool = "write"
path_prefix = "src/"
"#,
        )
        .unwrap()
    }

    #[test]
    fn bash_rule_requires_sandbox() {
        let r = rules();
        let input = json!({"command": "cargo test"});
        assert!(matches!(r.evaluate("bash", &input, true, false), Verdict::Auto { .. }));
        assert_eq!(r.evaluate("bash", &input, false, false), Verdict::Ask);
        // non-matching prefix asks
        assert_eq!(r.evaluate("bash", &json!({"command": "rm -rf /"}), true, false), Verdict::Ask);
    }

    #[test]
    fn protected_paths_never_auto() {
        let r = Rules::from_toml("[[allow]]\ntool = \"write\"\npath_prefix = \"\"\n").unwrap();
        // empty prefix matches everything — but protected still asks
        assert!(matches!(r.evaluate("write", &json!({"path": "src/a.rs"}), true, false), Verdict::Auto { .. }));
        assert_eq!(r.evaluate("write", &json!({"path": "Makefile"}), true, true), Verdict::Ask);
    }

    #[test]
    fn path_rules_and_defaults() {
        let r = rules();
        assert!(matches!(r.evaluate("write", &json!({"path": "./src/lib.rs"}), false, false), Verdict::Auto { .. }));
        assert_eq!(r.evaluate("write", &json!({"path": "docs/x.md"}), true, false), Verdict::Ask);
        assert_eq!(r.evaluate("edit", &json!({"path": "src/lib.rs"}), true, false), Verdict::Ask); // rule is write-only
        assert!(Rules::default().is_empty());
        assert!(Rules::from_toml("allow = 3").is_err());
    }
}
