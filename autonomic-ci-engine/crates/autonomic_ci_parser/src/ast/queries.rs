//! Tree-sitter S-expression queries used to extract structural features from
//! source code.
//!
//! Each `LanguageQueries` bundle contains three query groups:
//!   * **namespace**   – modules, packages, imports and use declarations
//!   * **structural** – structs, classes, types, traits and impl-like constructs
//!   * **functional** – functions, methods, closures and lambdas
//!
//! These strings are compiled into `tree_sitter::Query` objects by the
//! `matrix_mapper` module.

/// Per-language query bundle.
#[derive(Debug, Clone, Copy)]
pub struct LanguageQueries {
    pub language_name: &'static str,
    pub namespace: &'static str,
    pub structural: &'static str,
    pub functional: &'static str,
}

/// Rust query bundle.
pub const RUST: LanguageQueries = LanguageQueries {
    language_name: "rust",
    namespace: "(mod_item) @mod\n(use_declaration) @use",
    structural: "(struct_item) @struct\n(enum_item) @enum\n(type_item) @type\n(trait_item) @trait\n(impl_item) @impl",
    functional: "(function_item) @function\n(closure_expression) @closure",
};

/// Python query bundle.
pub const PYTHON: LanguageQueries = LanguageQueries {
    language_name: "python",
    namespace: "(module) @module\n(import_statement) @import\n(import_from_statement) @import_from",
    structural: "(class_definition) @class",
    functional: "(function_definition) @function\n(lambda) @lambda",
};

/// Go query bundle.
pub const GO: LanguageQueries = LanguageQueries {
    language_name: "go",
    namespace: "(package_clause) @package\n(import_spec) @import\n(import_declaration) @import_decl",
    structural: "(type_spec) @type\n(struct_type) @struct\n(interface_type) @interface",
    functional: "(function_declaration) @function\n(method_declaration) @method\n(func_literal) @func_lit",
};

/// Return the query bundle for a language name, or `None` if unsupported.
pub fn for_language(name: &str) -> Option<LanguageQueries> {
    match name {
        "rust" => Some(RUST),
        "python" => Some(PYTHON),
        "go" => Some(GO),
        _ => None,
    }
}
