//! Structural delta feature extraction using `tree-sitter`.
//!
//! `StructuralDeltaMatrixMapper` parses source code, runs tree-sitter query
//! cursors over structural S-expressions, and emits a numeric feature vector
//! `ΔX_i` describing changes in namespace, structural, and functional
//! density between two snapshots of a source file.

use std::error::Error;
use std::fmt;
use std::io;

use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

use super::queries;

/// Errors that can occur during AST-based feature mapping.
#[derive(Debug)]
pub enum ParserError {
    Io(io::Error),
    UnknownLanguage(String),
    TreeSitter(String),
}

impl fmt::Display for ParserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParserError::Io(e) => write!(f, "parser I/O error: {e}"),
            ParserError::UnknownLanguage(lang) => write!(f, "unsupported language: {lang}"),
            ParserError::TreeSitter(msg) => write!(f, "tree-sitter error: {msg}"),
        }
    }
}

impl Error for ParserError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ParserError::Io(e) => Some(e),
            ParserError::UnknownLanguage(_) | ParserError::TreeSitter(_) => None,
        }
    }
}

impl From<io::Error> for ParserError {
    fn from(err: io::Error) -> Self {
        ParserError::Io(err)
    }
}

/// A concrete numeric feature vector returned by the mapper.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureVector {
    pub labels: Vec<&'static str>,
    pub values: Vec<f64>,
}

impl FeatureVector {
    pub fn namespace(&self) -> f64 {
        self.values.first().copied().unwrap_or(0.0)
    }

    pub fn structural(&self) -> f64 {
        self.values.get(1).copied().unwrap_or(0.0)
    }

    pub fn functional(&self) -> f64 {
        self.values.get(2).copied().unwrap_or(0.0)
    }
}

/// Maps source-code changes into a dense numeric feature vector.
pub struct StructuralDeltaMatrixMapper {
    language: Language,
    namespace_query: Query,
    structural_query: Query,
    functional_query: Query,
}

impl StructuralDeltaMatrixMapper {
    /// Create a mapper for the named language ("rust", "python", or "go").
    pub fn new(language_name: &str) -> Result<Self, ParserError> {
        let language = language_from_name(language_name)
            .ok_or_else(|| ParserError::UnknownLanguage(language_name.to_string()))?;
        let queries = queries::for_language(language_name)
            .ok_or_else(|| ParserError::UnknownLanguage(language_name.to_string()))?;

        let namespace_query = compile_query(language, queries.namespace)?;
        let structural_query = compile_query(language, queries.structural)?;
        let functional_query = compile_query(language, queries.functional)?;

        Ok(Self {
            language,
            namespace_query,
            structural_query,
            functional_query,
        })
    }

    /// Return a feature vector for a single source snapshot.
    pub fn map(&self, source: &str) -> Result<FeatureVector, ParserError> {
        let counts = self.counts(source)?;
        Ok(FeatureVector {
            labels: vec!["namespace", "structural", "functional"],
            values: counts.into_iter().map(|c| c as f64).collect(),
        })
    }

    /// Return `ΔX = map(new_source) - map(old_source)` as a feature vector.
    pub fn map_delta(
        &self,
        old_source: &str,
        new_source: &str,
    ) -> Result<FeatureVector, ParserError> {
        let old = self.counts(old_source)?;
        let new = self.counts(new_source)?;

        let values: Vec<f64> = new
            .into_iter()
            .zip(old.into_iter())
            .map(|(n, o)| (n as f64) - (o as f64))
            .collect();

        Ok(FeatureVector {
            labels: vec!["namespace", "structural", "functional"],
            values,
        })
    }

    fn counts(&self, source: &str) -> Result<Vec<usize>, ParserError> {
        let tree = parse(self.language, source)?;
        let root = tree.root_node();

        let mut counts = vec![0usize; 3];
        counts[0] = count_captures(&self.namespace_query, root, source)?;
        counts[1] = count_captures(&self.structural_query, root, source)?;
        counts[2] = count_captures(&self.functional_query, root, source)?;

        Ok(counts)
    }
}

fn language_from_name(name: &str) -> Option<Language> {
    match name {
        "rust" => Some(tree_sitter_rust::language()),
        "python" => Some(tree_sitter_python::language()),
        "go" => Some(tree_sitter_go::language()),
        _ => None,
    }
}

fn compile_query(language: Language, source: &'static str) -> Result<Query, ParserError> {
    Query::new(language, source).map_err(|e| ParserError::TreeSitter(format!("{e}")))
}

fn parse(language: Language, source: &str) -> Result<tree_sitter::Tree, ParserError> {
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|e| ParserError::TreeSitter(format!("failed to set language: {e}")))?;
    parser
        .parse(source, None)
        .ok_or_else(|| ParserError::TreeSitter("parse failed".to_string()))
}

fn count_captures(query: &Query, root: Node, source: &str) -> Result<usize, ParserError> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source.as_bytes());
    let mut count = 0usize;

    while let Some(m) = matches.next() {
        count += m.captures.len();
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_delta_counts_functions_and_structs() {
        let mapper = StructuralDeltaMatrixMapper::new("rust").unwrap();

        let old = r#"
mod foo;

struct Bar;

fn baz() {}
"#;

        let new = r#"
mod foo;
mod qux;

struct Bar;
struct Baz;

fn baz() {}
fn qux() {}
"#;

        let delta = mapper.map_delta(old, new).unwrap();
        // namespace +1 (mod qux), structural +1 (struct Baz), functional +1 (fn qux)
        assert_eq!(delta.namespace(), 1.0);
        assert_eq!(delta.structural(), 1.0);
        assert_eq!(delta.functional(), 1.0);

        let baseline = mapper.map(old).unwrap();
        assert!(baseline.structural() >= 1.0);
        assert!(baseline.functional() >= 1.0);
    }
}
