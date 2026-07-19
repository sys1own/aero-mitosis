//! Wavefront scheduler for topological compilation bands.
//!
//! Independent compilation units are grouped into lock-free bands and executed
//! concurrently on the work-stealing pool. Each band finishes before the next
//! band starts, preserving dependency order while maximizing parallelism.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use autonomic_ci_parser::scm::ingestion::{SCMNode, StructuralCausalGraph};

use super::work_stealing::WorkStealingPool;

/// A single node of work derived from the structural causal graph.
#[derive(Debug, Clone)]
pub struct CompilationUnit {
    pub id: usize,
    pub name: String,
    pub language: String,
    pub dependencies: Vec<usize>,
}

impl From<&SCMNode> for CompilationUnit {
    fn from(node: &SCMNode) -> Self {
        Self {
            id: node.id,
            name: node.name.clone(),
            language: node.language.clone(),
            dependencies: Vec::new(),
        }
    }
}

/// Schedules compilation units in topological wavefront bands.
pub struct WavefrontScheduler {
    pool: WorkStealingPool,
}

impl WavefrontScheduler {
    /// Create a scheduler with the default number of worker threads.
    pub fn new() -> Self {
        Self {
            pool: WorkStealingPool::default_threads(),
        }
    }

    /// Create a scheduler with a specific number of worker threads.
    pub fn with_threads(threads: usize) -> Self {
        Self {
            pool: WorkStealingPool::new(threads.max(1)),
        }
    }

    /// Convert a graph into a topologically-ordered list of compilation units,
    /// wiring dependency edges from the graph.
    pub fn units_from_graph(graph: &StructuralCausalGraph) -> Vec<CompilationUnit> {
        let order = graph.topological_order().unwrap_or_default();
        let mut units: HashMap<usize, CompilationUnit> = order
            .iter()
            .filter_map(|&id| graph.nodes.get(id))
            .map(|node| (node.id, CompilationUnit::from(node)))
            .collect();

        for edge in &graph.edges {
            if let Some(unit) = units.get_mut(&edge.from) {
                unit.dependencies.push(edge.to);
            }
        }

        order.iter().filter_map(|&id| units.remove(&id)).collect()
    }

    /// Compute dependency-aware wavefront bands. Each band contains units whose
    /// remaining dependencies have all been satisfied by previous bands.
    fn bands(units: &[CompilationUnit]) -> Vec<Vec<CompilationUnit>> {
        let mut remaining: HashMap<usize, usize> =
            units.iter().map(|u| (u.id, u.dependencies.len())).collect();

        let by_id: HashMap<usize, &CompilationUnit> = units.iter().map(|u| (u.id, u)).collect();
        let mut dependents: HashMap<usize, Vec<usize>> = HashMap::new();
        for u in units {
            for &dep in &u.dependencies {
                dependents.entry(dep).or_default().push(u.id);
            }
        }

        let mut queue: VecDeque<usize> = units
            .iter()
            .filter(|u| remaining[&u.id] == 0)
            .map(|u| u.id)
            .collect();

        let mut bands: Vec<Vec<CompilationUnit>> = Vec::new();
        while !queue.is_empty() {
            let mut band = Vec::with_capacity(queue.len());
            for _ in 0..queue.len() {
                let id = queue.pop_front().unwrap();
                band.push((*by_id[&id]).clone());
                if let Some(children) = dependents.get(&id) {
                    for &child in children {
                        let count = remaining.get_mut(&child).unwrap();
                        *count -= 1;
                        if *count == 0 {
                            queue.push_back(child);
                        }
                    }
                }
            }
            bands.push(band);
        }

        bands
    }

    /// Run `work` over the graph in wavefront bands.
    ///
    /// Returns a vector of `(node_id, result)` pairs ordered by the graph's
    /// topological order.
    pub fn run<R, F>(&self, graph: &StructuralCausalGraph, work: F) -> Vec<(usize, R)>
    where
        R: Send + 'static,
        F: Fn(&CompilationUnit) -> R + Send + Sync + 'static,
    {
        let units = Self::units_from_graph(graph);
        self.run_units(&units, work)
    }

    /// Run `work` over a list of compilation units grouped into wavefront bands.
    pub fn run_units<R, F>(&self, units: &[CompilationUnit], work: F) -> Vec<(usize, R)>
    where
        R: Send + 'static,
        F: Fn(&CompilationUnit) -> R + Send + Sync + 'static,
    {
        let bands = Self::bands(units);
        let work = std::sync::Arc::new(work);
        let mut results: HashMap<usize, R> = HashMap::with_capacity(units.len());

        for band in bands {
            let (tx, rx) = crossbeam_channel::unbounded();
            for unit in &band {
                let tx = tx.clone();
                let unit = unit.clone();
                let work = Arc::clone(&work);
                self.pool.submit(move || {
                    let result = work(&unit);
                    let _ = tx.send((unit.id, result));
                });
            }
            drop(tx);

            while let Ok((id, result)) = rx.recv() {
                results.insert(id, result);
            }
        }

        // Return in topological order.
        units
            .iter()
            .filter_map(|u| results.remove(&u.id).map(|r| (u.id, r)))
            .collect()
    }
}

impl Default for WavefrontScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonomic_ci_parser::scm::ingestion::{DependencyType, SCMNode, StructuralCausalGraph};

    #[test]
    fn wavefront_runs_in_dependency_order() {
        let mut graph = StructuralCausalGraph::new();
        let a = graph.add_node(SCMNode {
            id: 0,
            name: "a".into(),
            path: Default::default(),
            language: "rust".into(),
            node_type: autonomic_ci_parser::scm::ingestion::NodeType::Package,
        });
        let b = graph.add_node(SCMNode {
            id: 0,
            name: "b".into(),
            path: Default::default(),
            language: "rust".into(),
            node_type: autonomic_ci_parser::scm::ingestion::NodeType::Package,
        });
        let c = graph.add_node(SCMNode {
            id: 0,
            name: "c".into(),
            path: Default::default(),
            language: "rust".into(),
            node_type: autonomic_ci_parser::scm::ingestion::NodeType::Package,
        });

        // a and b are independent; c depends on a and b.
        graph.add_edge(c, a, DependencyType::Compile);
        graph.add_edge(c, b, DependencyType::Compile);

        let scheduler = WavefrontScheduler::with_threads(2);
        let order: Vec<usize> = scheduler
            .run(&graph, |unit| unit.name.clone())
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        assert!(
            order.iter().position(|&id| id == c).unwrap()
                > order.iter().position(|&id| id == a).unwrap()
        );
        assert!(
            order.iter().position(|&id| id == c).unwrap()
                > order.iter().position(|&id| id == b).unwrap()
        );
    }
}
