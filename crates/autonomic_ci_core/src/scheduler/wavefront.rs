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
#[derive(Debug, Clone, PartialEq)]
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
    ///
    /// # Examples
    ///
    /// ```
    /// use autonomic_ci_core::scheduler::wavefront::WavefrontScheduler;
    /// use autonomic_ci_parser::scm::ingestion::StructuralCausalGraph;
    ///
    /// let graph = StructuralCausalGraph::new();
    /// let scheduler = WavefrontScheduler::new();
    /// let order: Vec<usize> = scheduler
    ///     .run(&graph, |unit| unit.id)
    ///     .into_iter()
    ///     .map(|(id, _)| id)
    ///     .collect();
    /// assert!(order.is_empty());
    /// ```
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
    use autonomic_ci_parser::scm::ingestion::{
        DependencyType, NodeType, SCMNode, StructuralCausalGraph,
    };

    fn make_node(graph: &mut StructuralCausalGraph, name: &str) -> usize {
        graph.add_node(SCMNode {
            id: 0,
            name: name.into(),
            path: Default::default(),
            language: "rust".into(),
            node_type: NodeType::Package,
        })
    }

    #[test]
    fn wavefront_runs_in_dependency_order() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");
        let b = make_node(&mut graph, "b");
        let c = make_node(&mut graph, "c");

        // c depends on a and b (edges point from dependent to dependency).
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

    #[test]
    fn empty_graph_yields_empty_bands() {
        let graph = StructuralCausalGraph::new();
        let scheduler = WavefrontScheduler::with_threads(1);
        let results: Vec<(usize, usize)> = scheduler.run(&graph, |_| 1);
        assert!(results.is_empty());
    }

    #[test]
    fn single_node_graph() {
        let mut graph = StructuralCausalGraph::new();
        let n = make_node(&mut graph, "solo");

        let scheduler = WavefrontScheduler::with_threads(1);
        let order: Vec<usize> = scheduler
            .run(&graph, |unit| unit.name.clone())
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        assert_eq!(order, vec![n]);
    }

    #[test]
    fn chain_of_five() {
        let mut graph = StructuralCausalGraph::new();
        let n0 = make_node(&mut graph, "n0");
        let n1 = make_node(&mut graph, "n1");
        let n2 = make_node(&mut graph, "n2");
        let n3 = make_node(&mut graph, "n3");
        let n4 = make_node(&mut graph, "n4");

        graph.add_edge(n1, n0, DependencyType::Compile);
        graph.add_edge(n2, n1, DependencyType::Compile);
        graph.add_edge(n3, n2, DependencyType::Compile);
        graph.add_edge(n4, n3, DependencyType::Compile);

        let scheduler = WavefrontScheduler::with_threads(2);
        let order: Vec<usize> = scheduler
            .run(&graph, |unit| unit.name.clone())
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        assert_eq!(order, vec![n0, n1, n2, n3, n4]);
    }

    #[test]
    fn diamond_graph_bands() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");
        let b = make_node(&mut graph, "b");
        let c = make_node(&mut graph, "c");
        let d = make_node(&mut graph, "d");

        // b and c depend on a; d depends on b and c.
        graph.add_edge(b, a, DependencyType::Compile);
        graph.add_edge(c, a, DependencyType::Compile);
        graph.add_edge(d, b, DependencyType::Compile);
        graph.add_edge(d, c, DependencyType::Compile);

        let units = WavefrontScheduler::units_from_graph(&graph);
        let bands = WavefrontScheduler::bands(&units);
        let band_names: Vec<Vec<&str>> = bands
            .iter()
            .map(|band| band.iter().map(|u| u.name.as_str()).collect())
            .collect();

        assert_eq!(band_names, vec![vec!["a"], vec!["b", "c"], vec!["d"]]);
    }

    #[test]
    fn deep_chain_completes() {
        let mut graph = StructuralCausalGraph::new();
        let mut prev = make_node(&mut graph, "n0");
        for i in 1..100 {
            let next = make_node(&mut graph, &format!("n{i}"));
            graph.add_edge(next, prev, DependencyType::Compile);
            prev = next;
        }

        let scheduler = WavefrontScheduler::with_threads(4);
        let order: Vec<usize> = scheduler
            .run(&graph, |unit| unit.name.clone())
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        assert_eq!(order.len(), 100);
        for (i, &id) in order.iter().enumerate() {
            assert_eq!(id, i);
        }
    }

    #[test]
    fn large_graph_does_not_stack_overflow() {
        let mut graph = StructuralCausalGraph::new();
        let root = make_node(&mut graph, "root");
        for i in 0..10_000 {
            let leaf = make_node(&mut graph, &format!("leaf_{i}"));
            graph.add_edge(leaf, root, DependencyType::Compile);
        }

        let units = WavefrontScheduler::units_from_graph(&graph);
        let bands = WavefrontScheduler::bands(&units);

        assert_eq!(bands.len(), 2);
        assert_eq!(bands[0].len(), 1);
        assert_eq!(bands[1].len(), 10_000);
    }

    #[test]
    fn self_cycle_produces_empty_bands() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");
        graph.add_edge(a, a, DependencyType::Compile);

        let units = WavefrontScheduler::units_from_graph(&graph);
        assert!(units.is_empty());
    }

    #[test]
    fn three_node_cycle_produces_empty_bands() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");
        let b = make_node(&mut graph, "b");
        let c = make_node(&mut graph, "c");

        graph.add_edge(a, b, DependencyType::Compile);
        graph.add_edge(b, c, DependencyType::Compile);
        graph.add_edge(c, a, DependencyType::Compile);

        let units = WavefrontScheduler::units_from_graph(&graph);
        assert!(units.is_empty());
    }

    #[test]
    fn node_waits_for_all_predecessors() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");
        let b = make_node(&mut graph, "b");
        let c = make_node(&mut graph, "c");
        let d = make_node(&mut graph, "d");

        graph.add_edge(d, a, DependencyType::Compile);
        graph.add_edge(d, b, DependencyType::Compile);
        graph.add_edge(d, c, DependencyType::Compile);

        let scheduler = WavefrontScheduler::with_threads(2);
        let order: Vec<usize> = scheduler
            .run(&graph, |unit| unit.name.clone())
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let d_pos = order.iter().position(|&id| id == d).unwrap();
        assert!(order.iter().position(|&id| id == a).unwrap() < d_pos);
        assert!(order.iter().position(|&id| id == b).unwrap() < d_pos);
        assert!(order.iter().position(|&id| id == c).unwrap() < d_pos);
    }

    #[test]
    fn zero_and_huge_thread_counts_are_safe() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");

        for threads in [0usize, usize::MAX] {
            let scheduler = WavefrontScheduler::with_threads(threads);
            let order: Vec<usize> = scheduler
                .run(&graph, |unit| unit.name.clone())
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(order, vec![a]);
        }
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn wavefront_bands_are_valid(
            node_count in 1usize..30usize,
            raw_edges in prop::collection::vec((1usize..30usize, 0usize..30usize), 0..60),
        ) {
            let mut graph = StructuralCausalGraph::new();
            let mut ids: Vec<usize> = Vec::with_capacity(node_count);
            for i in 0..node_count {
                ids.push(make_node(&mut graph, &format!("n{i}")));
            }

            // Only keep edges that point from a dependent to a dependency and
            // avoid self-loops so the graph is a DAG.
            for (from, to) in raw_edges {
                if from < node_count && to < node_count && from != to && to < from {
                    graph.add_edge(ids[from], ids[to], DependencyType::Compile);
                }
            }

            let units = WavefrontScheduler::units_from_graph(&graph);
            let bands = WavefrontScheduler::bands(&units);

            // Every node appears exactly once.
            let mut seen = std::collections::HashSet::new();
            for band in &bands {
                for unit in band {
                    assert!(seen.insert(unit.id), "node {} appears in multiple bands", unit.id);
                }
            }
            assert_eq!(seen.len(), units.len());

            // All predecessors must appear in an earlier band.
            let mut band_index: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();
            for (i, band) in bands.iter().enumerate() {
                for unit in band {
                    band_index.insert(unit.id, i);
                }
            }

            for unit in &units {
                for &dep in &unit.dependencies {
                    let unit_band = band_index[&unit.id];
                    let dep_band = band_index[&dep];
                    assert!(dep_band < unit_band,
                        "unit {} (band {}) has dependency {} (band {})",
                        unit.id, unit_band, dep, dep_band);
                }
            }
        }
    }
}
