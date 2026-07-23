//! Causal gradient analyzer for isolating root-cause variables behind
//! compilation failures.
//!
//! Given a `StructuralCausalGraph` and per-node AST feature deltas from the
//! parser, the analyzer computes a gradient score for each changed node. The
//! score balances the magnitude of structural change with the graph distance to
//! the failing compilation unit, highlighting the most likely statistical root
//! cause.

use std::collections::{HashMap, VecDeque};

use autonomic_ci_parser::ast::matrix_mapper::FeatureVector;
use autonomic_ci_parser::scm::ingestion::StructuralCausalGraph;
use serde::{Deserialize, Serialize};

/// A compiler failure event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilerFailure {
    pub node_id: Option<usize>,
    pub message: String,
    pub exit_code: Option<i32>,
}

/// A root-cause hypothesis produced by the causal gradient analyzer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalReport {
    pub failure: CompilerFailure,
    pub root_cause_node: Option<usize>,
    pub gradient_scores: HashMap<usize, f64>,
    pub explanation: String,
}

/// Computes causal gradient metrics from AST deltas and the SCM graph.
pub struct CausalGradientAnalyzer;

impl CausalGradientAnalyzer {
    /// Analyze a compiler failure and return the most likely root-cause node.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use autonomic_ci_core::diagnostics::causal_gradient::{CausalGradientAnalyzer, CompilerFailure};
    /// use autonomic_ci_parser::scm::ingestion::StructuralCausalGraph;
    ///
    /// let graph = StructuralCausalGraph::new();
    /// let failure = CompilerFailure {
    ///     node_id: Some(0),
    ///     message: "missing import".into(),
    ///     exit_code: Some(1),
    /// };
    /// let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &HashMap::new());
    /// assert!(report.gradient_scores.is_empty());
    /// ```
    pub fn diagnose(
        failure: &CompilerFailure,
        graph: &StructuralCausalGraph,
        deltas: &HashMap<usize, FeatureVector>,
    ) -> CausalReport {
        // If the failure node is explicitly present but not in the graph, the
        // failure context is inconsistent and we cannot compute gradients.
        if let Some(id) = failure.node_id {
            if graph.nodes.get(id).is_none() {
                return CausalReport {
                    failure: failure.clone(),
                    root_cause_node: None,
                    gradient_scores: HashMap::new(),
                    explanation: format!(
                        "Failure node {id} is not present in the causal graph; no gradient computed."
                    ),
                };
            }
        }

        let mut scores: HashMap<usize, f64> = HashMap::new();

        let distances = failure
            .node_id
            .map(|id| upstream_distances(graph, id))
            .unwrap_or_default();

        for (node_id, delta) in deltas {
            let magnitude = euclidean_norm(&delta.values);
            // Nodes with no upstream path to the failure are not considered causal.
            let score = if let Some(&distance) = distances.get(node_id) {
                magnitude / ((distance as f64) + 1.0)
            } else {
                0.0
            };
            scores.insert(*node_id, score);
        }

        let root_cause = scores
            .iter()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .and_then(|(&id, &score)| {
                if score > 0.0 && score.is_finite() {
                    Some(id)
                } else {
                    None
                }
            });

        let explanation = if let (Some(fail_id), Some(cause_id)) = (failure.node_id, root_cause) {
            format!(
                "Node {cause_id} has the strongest causal gradient relative to failure {fail_id}."
            )
        } else if let Some(cause_id) = root_cause {
            format!("Node {cause_id} has the strongest causal gradient.")
        } else {
            "No AST deltas were available to compute a causal gradient.".into()
        };

        CausalReport {
            failure: failure.clone(),
            root_cause_node: root_cause,
            gradient_scores: scores,
            explanation,
        }
    }
}

fn sanitize(value: f64) -> f64 {
    if value.is_nan() {
        f64::MAX
    } else if value.is_infinite() {
        if value.is_sign_positive() {
            f64::MAX
        } else {
            0.0
        }
    } else {
        value.max(0.0)
    }
}

fn euclidean_norm(values: &[f64]) -> f64 {
    let sum = values.iter().map(|&v| sanitize(v).powi(2)).sum::<f64>();
    if sum.is_infinite() || sum.is_nan() {
        f64::MAX
    } else {
        sum.sqrt()
    }
}

/// Compute the shortest graph distance from `failure_id` to every other node,
/// following dependency edges (failure -> dependency). This captures how many
/// layers upstream a candidate root cause is.
fn upstream_distances(graph: &StructuralCausalGraph, failure_id: usize) -> HashMap<usize, usize> {
    let mut adj: HashMap<usize, Vec<usize>> = HashMap::new();
    for edge in &graph.edges {
        adj.entry(edge.from).or_default().push(edge.to);
    }

    let mut distances = HashMap::new();
    let mut queue = VecDeque::new();
    distances.insert(failure_id, 0usize);
    queue.push_back(failure_id);

    while let Some(current) = queue.pop_front() {
        let d = distances[&current];
        if let Some(neighbors) = adj.get(&current) {
            for &next in neighbors {
                if let std::collections::hash_map::Entry::Vacant(e) = distances.entry(next) {
                    e.insert(d + 1);
                    queue.push_back(next);
                }
            }
        }
    }

    distances
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

    fn delta(values: &[f64]) -> FeatureVector {
        FeatureVector {
            labels: vec!["namespace", "structural", "functional"],
            values: values.to_vec(),
        }
    }

    #[test]
    fn root_cause_is_upstream_dependency() {
        let mut graph = StructuralCausalGraph::new();
        let lib = make_node(&mut graph, "lib");
        let app = make_node(&mut graph, "app");
        graph.add_edge(app, lib, DependencyType::Compile);

        let mut deltas = HashMap::new();
        deltas.insert(lib, delta(&[0.0, 3.0, 1.0]));
        deltas.insert(app, delta(&[0.0, 0.5, 0.0]));

        let failure = CompilerFailure {
            node_id: Some(app),
            message: "type mismatch".into(),
            exit_code: Some(101),
        };

        let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &deltas);
        assert_eq!(report.root_cause_node, Some(lib));
        assert!(report.gradient_scores[&lib] > report.gradient_scores[&app]);
    }

    #[test]
    fn two_independent_failures_are_ranked() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");
        let b = make_node(&mut graph, "b");
        let fail = make_node(&mut graph, "fail");

        graph.add_edge(fail, a, DependencyType::Compile);
        graph.add_edge(fail, b, DependencyType::Compile);

        let mut deltas = HashMap::new();
        deltas.insert(a, delta(&[10.0, 0.0, 0.0]));
        deltas.insert(b, delta(&[5.0, 0.0, 0.0]));

        let failure = CompilerFailure {
            node_id: Some(fail),
            message: "error".into(),
            exit_code: Some(1),
        };

        let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &deltas);
        assert_eq!(report.root_cause_node, Some(a));
        assert!(report.gradient_scores[&a] > report.gradient_scores[&b]);
        assert!(report.gradient_scores[&b] > 0.0);
    }

    #[test]
    fn missing_failure_node_produces_empty_report() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");

        let mut deltas = HashMap::new();
        deltas.insert(a, delta(&[1.0, 2.0, 3.0]));

        let failure = CompilerFailure {
            node_id: Some(999),
            message: "unknown".into(),
            exit_code: Some(1),
        };

        let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &deltas);
        assert!(report.gradient_scores.is_empty());
        assert!(report.root_cause_node.is_none());
        assert!(report.explanation.contains("not present"));
    }

    #[test]
    fn all_zero_deltas_yield_zero_scores() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");
        let b = make_node(&mut graph, "b");
        graph.add_edge(b, a, DependencyType::Compile);

        let mut deltas = HashMap::new();
        deltas.insert(a, delta(&[0.0, 0.0, 0.0]));
        deltas.insert(b, delta(&[0.0, 0.0, 0.0]));

        let failure = CompilerFailure {
            node_id: Some(b),
            message: "fail".into(),
            exit_code: Some(1),
        };

        let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &deltas);
        assert!(report.gradient_scores.values().all(|&s| s == 0.0));
        assert!(report.root_cause_node.is_none());
    }

    #[test]
    fn negative_deltas_are_clamped_to_zero() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");

        let mut deltas = HashMap::new();
        deltas.insert(a, delta(&[-2.0, -3.0, -4.0]));

        let failure = CompilerFailure {
            node_id: Some(a),
            message: "fail".into(),
            exit_code: Some(1),
        };

        let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &deltas);
        assert_eq!(report.gradient_scores[&a], 0.0);
        assert!(report.root_cause_node.is_none());
    }

    #[test]
    fn large_graph_distance_is_correct() {
        let mut graph = StructuralCausalGraph::new();
        let mut prev = make_node(&mut graph, "n0");
        for i in 1..1000 {
            let next = make_node(&mut graph, &format!("n{i}"));
            graph.add_edge(next, prev, DependencyType::Compile);
            prev = next;
        }

        let failure_id = prev;
        let mut deltas = HashMap::new();
        deltas.insert(0, delta(&[1.0, 0.0, 0.0]));

        let failure = CompilerFailure {
            node_id: Some(failure_id),
            message: "fail".into(),
            exit_code: Some(1),
        };

        let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &deltas);
        // Distance from the failing node back to node 0 is 999 edges.
        let expected = 1.0 / 1000.0;
        assert!((report.gradient_scores[&0] - expected).abs() < 1e-12);
    }

    #[test]
    fn nan_and_inf_values_do_not_panic() {
        let mut graph = StructuralCausalGraph::new();
        let a = make_node(&mut graph, "a");
        let b = make_node(&mut graph, "b");
        graph.add_edge(b, a, DependencyType::Compile);

        let mut deltas = HashMap::new();
        deltas.insert(a, delta(&[f64::NAN, f64::INFINITY, f64::NEG_INFINITY]));

        let failure = CompilerFailure {
            node_id: Some(b),
            message: "fail".into(),
            exit_code: Some(1),
        };

        let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &deltas);
        assert!(!report.gradient_scores[&a].is_nan());
        assert!(report.gradient_scores[&a] >= 0.0);
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn scores_are_non_negative_and_zero_when_unreachable(
            node_count in 1usize..20usize,
            raw_edges in prop::collection::vec((1usize..20usize, 0usize..20usize), 0..40),
            failure_idx in 0usize..20usize,
            raw_values in prop::collection::vec(any::<f64>(), 0..60),
        ) {
            let mut graph = StructuralCausalGraph::new();
            let mut ids: Vec<usize> = Vec::with_capacity(node_count);
            for i in 0..node_count {
                ids.push(make_node(&mut graph, &format!("n{i}")));
            }

            for (from, to) in raw_edges {
                if from < node_count && to < node_count && from != to && to < from {
                    graph.add_edge(ids[from], ids[to], DependencyType::Compile);
                }
            }

            let mut deltas = HashMap::new();
            for i in 0..node_count {
                let start = i * 3;
                let values = if start + 3 <= raw_values.len() {
                    &raw_values[start..start + 3]
                } else {
                    &[1.0, 1.0, 1.0][..]
                };
                deltas.insert(ids[i], delta(values));
            }

            let failure_node = if failure_idx < node_count { Some(ids[failure_idx]) } else { None };
            let failure = CompilerFailure {
                node_id: failure_node,
                message: "property test".into(),
                exit_code: Some(1),
            };

            let distances = failure_node
                .map(|id| upstream_distances(&graph, id))
                .unwrap_or_default();

            let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &deltas);

            for (node_id, score) in &report.gradient_scores {
                assert!(!score.is_nan(), "score for {node_id} is NaN");
                assert!(*score >= 0.0, "score for {node_id} is negative");
                if !distances.contains_key(node_id) {
                    assert_eq!(*score, 0.0, "unreachable node {node_id} has non-zero score");
                }
            }
        }
    }
}
