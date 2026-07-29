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

    /// Is this a statement-class node whose terminator is optional — i.e. one
    /// where the tree, not a lexical guess, tells us ASI fired (D-B7)?
    ///
    /// An omission here costs coverage, never correctness: a statement boundary
    /// we fail to record joins plainly, and if that changes the meaning the
    /// AST-shape check refuses the whole minify.
    pub(crate) fn is_asi_statement(self, node_kind: &str) -> bool {
        /// Shared by JS and TS.
        const SCRIPT: &[&str] = &[
            "expression_statement",
            "lexical_declaration",
            "variable_declaration",
            "return_statement",
            "break_statement",
            "continue_statement",
            "throw_statement",
            "do_statement",
            "import_statement",
            "export_statement",
            "debugger_statement",
            "field_definition",
        ];
        /// TS's type-level members, whose `;`/`,` is equally optional.
        const TYPED: &[&str] = &[
            "type_alias_declaration",
            "import_alias",
            "property_signature",
            "method_signature",
            "public_field_definition",
            "abstract_method_signature",
            "index_signature",
            "call_signature",
            "construct_signature",
        ];
        match self {
            Lang::JavaScript => SCRIPT.contains(&node_kind),
            Lang::TypeScript | Lang::Tsx => {
                SCRIPT.contains(&node_kind) || TYPED.contains(&node_kind)
            }
            Lang::Rust | Lang::Go | Lang::Python => false,
        }
    }

    /// Is this the body of a block, class or switch — the one kind of `}` that
    /// terminates its enclosing statement without a `;`?
    ///
    /// An object literal's `}` is not, which is why this asks the tree instead of
    /// looking at the statement's last character. A kind missing here costs a
    /// spurious `;`, which the AST-shape check catches as an added
    /// `empty_statement`.
    pub(crate) fn is_body(self, node_kind: &str) -> bool {
        match self {
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
                node_kind,
                "statement_block"
                    | "class_body"
                    | "switch_body"
                    | "enum_body"
                    | "interface_body"
                    | "module"
                    | "internal_module"
            ),
            Lang::Rust | Lang::Go | Lang::Python => false,
        }
    }

    /// Does this subtree own its inter-token whitespace, so the joiner must copy
    /// its gaps rather than synthesize them? `jsx_text` carries its own spacing,
    /// but attribute lists do not: `class="a" id="b"` has no re-lex hazard the
    /// separator table can see, and joining it is invalid.
    pub(crate) fn owns_its_whitespace(self, node_kind: &str) -> bool {
        match self {
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
                node_kind,
                "jsx_element" | "jsx_fragment" | "jsx_self_closing_element"
            ),
            Lang::Rust | Lang::Go | Lang::Python => false,
        }
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
