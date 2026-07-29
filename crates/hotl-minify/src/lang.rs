//! The language registry: extension → grammar, plus the per-grammar node-kind
//! tables the joiner consults.

/// A language this crate can minify. Tsx is separate from TypeScript because
/// TS-with-JSX is a distinct grammar, not a flag on the TS one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Go,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
}

pub fn language_for_path(path: &str) -> Option<Lang> {
    match std::path::Path::new(path).extension()?.to_str()? {
        "rs" => Some(Lang::Rust),
        "go" => Some(Lang::Go),
        "py" | "pyi" => Some(Lang::Python),
        "js" | "mjs" | "cjs" | "jsx" => Some(Lang::JavaScript), // the JS grammar parses JSX too
        "ts" | "mts" | "cts" => Some(Lang::TypeScript),
        "tsx" => Some(Lang::Tsx),
        _ => None,
    }
}

impl Lang {
    pub(crate) fn grammar(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    pub(crate) fn is_comment(self, node_kind: &str) -> bool {
        matches!(
            node_kind,
            "comment" | "line_comment" | "block_comment" | "doc_comment" | "html_comment"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_extension_maps_to_a_grammar_that_loads() {
        for path in [
            "a.rs", "a.go", "a.py", "a.js", "a.mjs", "a.cjs", "a.jsx", "a.ts", "a.mts", "a.cts",
            "a.tsx",
        ] {
            let lang = language_for_path(path).unwrap_or_else(|| panic!("no lang for {path}"));
            let mut p = tree_sitter::Parser::new();
            p.set_language(&lang.grammar())
                .unwrap_or_else(|e| panic!("{path}: {lang:?} grammar rejected: {e}"));
        }
    }

    #[test]
    fn an_unsupported_extension_has_no_language() {
        assert_eq!(language_for_path("notes.md"), None);
        assert_eq!(language_for_path("Makefile"), None);
    }
}
