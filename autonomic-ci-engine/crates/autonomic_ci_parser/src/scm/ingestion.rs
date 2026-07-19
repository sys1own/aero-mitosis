//! SCM (Source/Structural Causal Model) ingestion.
//!
//! Discovers repository layouts, parses dependency manifests, and builds a
//! language-agnostic `StructuralCausalGraph`.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Kinds of nodes that can appear in the causal graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeType {
    Package,
    Module,
    External,
    SourceDirectory,
}

/// Kinds of dependencies that can connect two SCM nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DependencyType {
    Compile,
    Runtime,
    Dev,
    Feature,
    Structural,
}

/// A node in the structural causal graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SCMNode {
    pub id: usize,
    pub name: String,
    pub path: PathBuf,
    pub language: String,
    pub node_type: NodeType,
}

/// A directed edge in the structural causal graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SCMEdge {
    pub from: usize,
    pub to: usize,
    pub dependency_type: DependencyType,
}

/// A language-agnostic DAG representing packages and their dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuralCausalGraph {
    pub nodes: Vec<SCMNode>,
    pub edges: Vec<SCMEdge>,
    #[serde(skip)]
    by_name: HashMap<String, usize>,
}

impl Default for StructuralCausalGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl StructuralCausalGraph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            by_name: HashMap::new(),
        }
    }

    /// Add a node and return its stable id. If a node with the same name
    /// already exists, the existing id is returned and the new node is dropped.
    pub fn add_node(&mut self, node: SCMNode) -> usize {
        if let Some(&id) = self.by_name.get(&node.name) {
            return id;
        }
        let id = self.nodes.len();
        let mut node = node;
        node.id = id;
        self.by_name.insert(node.name.clone(), id);
        self.nodes.push(node);
        id
    }

    /// Ensure a node with `name` exists. If it does not, create an external
    /// placeholder node.
    pub fn ensure_node(&mut self, name: &str, node_type: NodeType, language: &str, path: &Path) -> usize {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        self.add_node(SCMNode {
            id: 0,
            name: name.to_string(),
            path: path.to_path_buf(),
            language: language.to_string(),
            node_type,
        })
    }

    pub fn node_by_name(&self, name: &str) -> Option<&SCMNode> {
        self.by_name.get(name).and_then(|&id| self.nodes.get(id))
    }

    pub fn add_edge(&mut self, from: usize, to: usize, dependency_type: DependencyType) {
        self.edges.push(SCMEdge {
            from,
            to,
            dependency_type,
        });
    }

    /// Return a topological ordering of package ids using Kahn's algorithm.
    /// External nodes are included but have no outgoing edges.
    pub fn topological_order(&self) -> Option<Vec<usize>> {
        // Edges point from a package to its dependency. A valid build order
        // processes dependencies before the packages that depend on them.
        let mut in_degree = vec![0; self.nodes.len()];
        let mut dependents: HashMap<usize, Vec<usize>> = HashMap::new();
        for edge in &self.edges {
            in_degree[edge.from] += 1;
            dependents.entry(edge.to).or_default().push(edge.from);
        }

        let mut queue: VecDeque<usize> = self
            .nodes
            .iter()
            .map(|n| n.id)
            .filter(|&id| in_degree[id] == 0)
            .collect();

        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = queue.pop_front() {
            order.push(id);
            if let Some(children) = dependents.get(&id) {
                for &next in children {
                    in_degree[next] -= 1;
                    if in_degree[next] == 0 {
                        queue.push_back(next);
                    }
                }
            }
        }

        if order.len() == self.nodes.len() {
            Some(order)
        } else {
            None
        }
    }
}

/// Errors that can occur while ingesting an SCM graph.
#[derive(Debug)]
pub enum IngestionError {
    Io(io::Error),
    Toml(toml::de::Error),
    Message(String),
}

impl fmt::Display for IngestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IngestionError::Io(e) => write!(f, "SCM ingestion I/O error: {e}"),
            IngestionError::Toml(e) => write!(f, "SCM ingestion TOML error: {e}"),
            IngestionError::Message(msg) => write!(f, "SCM ingestion error: {msg}"),
        }
    }
}

impl std::error::Error for IngestionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IngestionError::Io(e) => Some(e),
            IngestionError::Toml(e) => Some(e),
            IngestionError::Message(_) => None,
        }
    }
}

impl From<io::Error> for IngestionError {
    fn from(err: io::Error) -> Self {
        IngestionError::Io(err)
    }
}

impl From<toml::de::Error> for IngestionError {
    fn from(err: toml::de::Error) -> Self {
        IngestionError::Toml(err)
    }
}

/// Discovers code targets and builds a `StructuralCausalGraph`.
pub struct IngestionEngine;

impl IngestionEngine {
    /// Walk `root`, parse dependency manifests, and produce a causal graph.
    pub fn discover(root: &Path) -> Result<StructuralCausalGraph, IngestionError> {
        let mut graph = StructuralCausalGraph::new();
        Self::discover_dir(root, &mut graph, root)?;
        Ok(graph)
    }

    fn discover_dir(
        current: &Path,
        graph: &mut StructuralCausalGraph,
        root: &Path,
    ) -> Result<(), IngestionError> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "target" || name == "node_modules" {
                    continue;
                }
                Self::discover_dir(&path, graph, root)?;
            } else if file_type.is_file() {
                if let Some((package_name, language, deps)) = Self::parse_manifest(&path)? {
                    let dir = path.parent().unwrap_or(root);
                    let node_id = graph.ensure_node(
                        &package_name,
                        NodeType::Package,
                        &language,
                        dir,
                    );
                    // Update the existing node (which may have been a placeholder)
                    // with the real path and language.
                    if let Some(node) = graph.nodes.get_mut(node_id) {
                        node.path = dir.to_path_buf();
                        node.language = language.clone();
                        node.node_type = NodeType::Package;
                    }
                    for (dep_name, dep_type) in deps {
                        let dep_id = graph.ensure_node(&dep_name, NodeType::External, "", dir);
                        graph.add_edge(node_id, dep_id, dep_type);
                    }
                }
            }
        }
        Ok(())
    }

    fn parse_manifest(
        path: &Path,
    ) -> Result<Option<(String, String, Vec<(String, DependencyType)>)>, IngestionError> {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            match name {
                "Cargo.toml" => Self::parse_cargo_toml(path),
                "go.mod" => Self::parse_go_mod(path),
                "pyproject.toml" => Self::parse_pyproject_toml(path),
                "requirements.txt" => Self::parse_requirements_txt(path),
                _ => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    fn parse_cargo_toml(
        path: &Path,
    ) -> Result<Option<(String, String, Vec<(String, DependencyType)>)>, IngestionError> {
        let content = fs::read_to_string(path)?;
        let value: toml::Value = toml::from_str(&content)?;

        let name = value
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            return Ok(None);
        }

        let mut deps = Vec::new();
        let dep_tables = [
            ("dependencies", DependencyType::Compile),
            ("dev-dependencies", DependencyType::Dev),
            ("build-dependencies", DependencyType::Dev),
        ];
        for (table, dep_type) in dep_tables {
            if let Some(table) = value.get(table).and_then(|t| t.as_table()) {
                for dep_name in table.keys() {
                    deps.push((dep_name.clone(), dep_type));
                }
            }
        }

        Ok(Some((name, "rust".to_string(), deps)))
    }

    fn parse_go_mod(
        path: &Path,
    ) -> Result<Option<(String, String, Vec<(String, DependencyType)>)>, IngestionError> {
        let content = fs::read_to_string(path)?;
        let mut name = String::new();
        let mut deps = Vec::new();
        let mut in_require = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("module ") {
                name = trimmed
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
            } else if trimmed.starts_with("require (") {
                in_require = true;
            } else if trimmed == ")" {
                in_require = false;
            } else if in_require {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if !parts.is_empty() && !parts[0].starts_with("//") {
                    deps.push((parts[0].to_string(), DependencyType::Compile));
                }
            } else if trimmed.starts_with("require ") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    deps.push((parts[1].to_string(), DependencyType::Compile));
                }
            }
        }

        if name.is_empty() {
            Ok(None)
        } else {
            Ok(Some((name, "go".to_string(), deps)))
        }
    }

    fn parse_pyproject_toml(
        path: &Path,
    ) -> Result<Option<(String, String, Vec<(String, DependencyType)>)>, IngestionError> {
        let content = fs::read_to_string(path)?;
        let value: toml::Value = toml::from_str(&content)?;

        let name = value
            .get("project")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .or_else(|| {
                value
                    .get("tool")
                    .and_then(|t| t.get("poetry"))
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
            })
            .unwrap_or("")
            .to_string();

        if name.is_empty() {
            return Ok(None);
        }

        let mut deps = Vec::new();

        // PEP 621 project.dependencies
        if let Some(arr) = value
            .get("project")
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_array())
        {
            for item in arr {
                if let Some(spec) = item.as_str() {
                    deps.push((parse_pkg_name(spec), DependencyType::Runtime));
                }
            }
        }

        // PEP 621 project.optional-dependencies (each maps to a feature group)
        if let Some(table) = value
            .get("project")
            .and_then(|p| p.get("optional-dependencies"))
            .and_then(|d| d.as_table())
        {
            for group in table.values() {
                if let Some(arr) = group.as_array() {
                    for item in arr {
                        if let Some(spec) = item.as_str() {
                            deps.push((parse_pkg_name(spec), DependencyType::Feature));
                        }
                    }
                }
            }
        }

        // Poetry-style dependencies
        if let Some(table) = value
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_table())
        {
            for (dep_name, _) in table {
                if dep_name == "python" {
                    continue;
                }
                deps.push((dep_name.clone(), DependencyType::Runtime));
            }
        }

        if let Some(table) = value
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("dev-dependencies"))
            .and_then(|d| d.as_table())
        {
            for (dep_name, _) in table {
                deps.push((dep_name.clone(), DependencyType::Dev));
            }
        }

        Ok(Some((name, "python".to_string(), deps)))
    }

    fn parse_requirements_txt(
        path: &Path,
    ) -> Result<Option<(String, String, Vec<(String, DependencyType)>)>, IngestionError> {
        let dir_name = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if dir_name.is_empty() {
            return Ok(None);
        }

        let content = fs::read_to_string(path)?;
        let mut deps = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
                continue;
            }
            deps.push((parse_pkg_name(line), DependencyType::Runtime));
        }

        Ok(Some((dir_name, "python".to_string(), deps)))
    }
}

/// Extract the package name from a dependency specification such as
/// `requests>=2.28` or `numpy==1.23 ; python_version >= "3.8"`.
fn parse_pkg_name(spec: &str) -> String {
    let spec = spec.split(';').next().unwrap_or(spec).trim();
    for (i, c) in spec.char_indices() {
        if c.is_whitespace()
            || c == '='
            || c == '<'
            || c == '>'
            || c == '!'
            || c == '~'
            || c == '['
        {
            return spec[..i].trim().to_string();
        }
    }
    spec.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_cargo_and_python_deps() {
        let base = std::env::temp_dir().join(format!("aci_scm_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);

        let rust_dir = base.join("rust");
        fs::create_dir_all(&rust_dir).unwrap();
        fs::write(
            rust_dir.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1"

[dev-dependencies]
tokio = { version = "1", features = ["full"] }
"#,
        )
        .unwrap();

        let py_dir = base.join("py");
        fs::create_dir_all(&py_dir).unwrap();
        fs::write(
            py_dir.join("pyproject.toml"),
            r#"
[project]
name = "py-demo"
version = "0.1.0"
dependencies = ["requests>=2.28"]
"#,
        )
        .unwrap();

        let graph = IngestionEngine::discover(&base).unwrap();
        assert!(graph.node_by_name("demo").is_some());
        assert!(graph.node_by_name("py-demo").is_some());
        assert!(graph.node_by_name("serde").is_some());
        assert!(graph.node_by_name("requests").is_some());

        let demo = graph.node_by_name("demo").unwrap();
        let serde_node = graph.node_by_name("serde").unwrap();
        let has_serde_edge = graph.edges.iter().any(|e| {
            e.from == demo.id && e.to == serde_node.id && e.dependency_type == DependencyType::Compile
        });
        assert!(has_serde_edge);

        let _ = fs::remove_dir_all(&base);
    }
}
