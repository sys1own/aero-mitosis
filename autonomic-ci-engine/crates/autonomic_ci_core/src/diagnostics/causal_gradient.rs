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
    pub fn diagnose(
        failure: &CompilerFailure,
        graph: &StructuralCausalGraph,
        deltas: &HashMap<usize, FeatureVector>,
    ) -> CausalReport {
        let mut scores: HashMap<usize, f64> = HashMap::new();

        let distances = failure
            .node_id
            .map(|id| upstream_distances(graph, id))
            .unwrap_or_default();

        for (node_id, delta) in deltas {
            let magnitude = euclidean_norm(&delta.values);
            let distance = distances.get(node_id).copied().unwrap_or(usize::MAX);
            // Prefer large changes close to the failing node (smaller distance).
            // Add 1 to avoid division by zero for the failure node itself.
            let score = magnitude / ((distance as f64) + 1.0);
            scores.insert(*node_id, score);
        }

        let root_cause = scores
            .iter()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(&id, _)| id);

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

fn euclidean_norm(values: &[f64]) -> f64 {
    values.iter().map(|v| v * v).sum::<f64>().sqrt()
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
                if !distances.contains_key(&next) {
                    distances.insert(next, d + 1);
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
    use autonomic_ci_parser::scm::ingestion::{DependencyType, SCMNode, StructuralCausalGraph};

    #[test]
    fn root_cause_is_upstream_dependency() {
        let mut graph = StructuralCausalGraph::new();
        let lib = graph.add_node(SCMNode {
            id: 0,
            name: "lib".into(),
            path: Default::default(),
            language: "rust".into(),
            node_type: autonomic_ci_parser::scm::ingestion::NodeType::Package,
        });
        let app = graph.add_node(SCMNode {
            id: 0,
            name: "app".into(),
            path: Default::default(),
            language: "rust".into(),
            node_type: autonomic_ci_parser::scm::ingestion::NodeType::Package,
        });
        graph.add_edge(app, lib, DependencyType::Compile);

        let mut deltas = HashMap::new();
        deltas.insert(
            lib,
            FeatureVector {
                labels: vec!["namespace", "structural", "functional"],
                values: vec![0.0, 3.0, 1.0],
            },
        );
        deltas.insert(
            app,
            FeatureVector {
                labels: vec!["namespace", "structural", "functional"],
                values: vec![0.0, 0.5, 0.0],
            },
        );

        let failure = CompilerFailure {
            node_id: Some(app),
            message: "type mismatch".into(),
            exit_code: Some(101),
        };

        let report = CausalGradientAnalyzer::diagnose(&failure, &graph, &deltas);
        assert_eq!(report.root_cause_node, Some(lib));
        assert!(report.gradient_scores[&lib] > report.gradient_scores[&app]);
    }
}
