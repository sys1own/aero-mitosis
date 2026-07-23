//! End-to-end integration tests for the autonomic CI self-healing router.
//!
//! These tests exercise the full repair loop: causal graph construction,
//! overlay virtualization, patch validation, penalty-index gating, and
//! atomic commit back to the host workspace.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use autonomic_ci_core::diagnostics::causal_gradient::CompilerFailure;
use autonomic_ci_core::self_healing::router::{
    Patch, PatchGenerator, PenaltyWeights, SelfHealingConfig, SelfHealingRouter,
    StaticPatchGenerator, ValidationBaseline, ValidationResult, Validator,
};
use autonomic_ci_parser::ast::matrix_mapper::FeatureVector;
use autonomic_ci_parser::scm::ingestion::{NodeType, SCMNode, StructuralCausalGraph};

struct MockValidator;

impl Validator for MockValidator {
    fn validate(&self, root: &Path) -> ValidationResult {
        let content = fs::read_to_string(root.join("config.txt")).unwrap_or_default();

        if content.contains("broken=true") {
            ValidationResult {
                success: false,
                elapsed_ms: 50,
                output_size: 0,
                stderr: "config is still broken".into(),
            }
        } else if content.contains("expensive=true") {
            ValidationResult {
                success: true,
                elapsed_ms: 200,
                output_size: 200_000,
                stderr: String::new(),
            }
        } else if content.contains("fixed=true") {
            ValidationResult {
                success: true,
                elapsed_ms: 50,
                output_size: 100,
                stderr: String::new(),
            }
        } else {
            ValidationResult {
                success: false,
                elapsed_ms: 0,
                output_size: 0,
                stderr: "unrecognized config".into(),
            }
        }
    }
}

fn make_baseline(content: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "aero_e2e_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("config.txt"), content).unwrap();
    base
}

fn make_graph(node_id: usize) -> StructuralCausalGraph {
    let mut graph = StructuralCausalGraph::new();
    graph.add_node(SCMNode {
        id: node_id,
        name: "demo".into(),
        path: Default::default(),
        language: "rust".into(),
        node_type: NodeType::Package,
    });
    graph
}

fn make_deltas(node_id: usize) -> HashMap<usize, FeatureVector> {
    let mut deltas = HashMap::new();
    deltas.insert(
        node_id,
        FeatureVector {
            labels: vec!["namespace", "structural", "functional"],
            values: vec![0.0, 1.0, 0.0],
        },
    );
    deltas
}

#[tokio::test]
async fn repairs_broken_config_and_commits() {
    let baseline = make_baseline("broken=true\n");

    let graph = make_graph(0);
    let deltas = make_deltas(0);
    let failure = CompilerFailure {
        node_id: Some(0),
        message: "broken config".into(),
        exit_code: Some(1),
    };

    let generator: Arc<dyn PatchGenerator> = Arc::new(StaticPatchGenerator {
        patches: vec![Patch {
            path: PathBuf::from("config.txt"),
            content: "fixed=true\n".into(),
        }],
    });

    let config = SelfHealingConfig {
        weights: PenaltyWeights {
            w_time: 1.0,
            w_size: 0.001,
        },
        penalty_threshold: 1000.0,
        max_retries: 3,
    };
    let router = SelfHealingRouter::new(config);
    let baseline_metrics = ValidationBaseline {
        time_ms: 100,
        output_size: 100,
    };

    let report = router
        .repair(
            &baseline,
            &failure,
            &graph,
            &deltas,
            &baseline_metrics,
            Arc::clone(&generator),
            Arc::new(MockValidator),
        )
        .await
        .expect("repair should succeed");

    assert!(report.committed);
    assert!(report.penalty <= config.penalty_threshold);

    let final_content = fs::read_to_string(baseline.join("config.txt")).unwrap();
    assert!(final_content.contains("fixed=true"));
}

#[tokio::test]
async fn discards_failing_and_overbudget_candidates_then_commits() {
    let baseline = make_baseline("broken=true\n");

    let graph = make_graph(0);
    let deltas = make_deltas(0);
    let failure = CompilerFailure {
        node_id: Some(0),
        message: "broken config".into(),
        exit_code: Some(1),
    };

    let generator: Arc<dyn PatchGenerator> = Arc::new(StaticPatchGenerator {
        patches: vec![
            Patch {
                path: PathBuf::from("config.txt"),
                content: "broken=true\n".into(),
            },
            Patch {
                path: PathBuf::from("config.txt"),
                content: "expensive=true\n".into(),
            },
            Patch {
                path: PathBuf::from("config.txt"),
                content: "fixed=true\n".into(),
            },
        ],
    });

    let config = SelfHealingConfig {
        weights: PenaltyWeights {
            w_time: 1.0,
            w_size: 0.001,
        },
        penalty_threshold: 100.0,
        max_retries: 3,
    };
    let router = SelfHealingRouter::new(config);
    let baseline_metrics = ValidationBaseline {
        time_ms: 100,
        output_size: 100,
    };

    let report = router
        .repair(
            &baseline,
            &failure,
            &graph,
            &deltas,
            &baseline_metrics,
            Arc::clone(&generator),
            Arc::new(MockValidator),
        )
        .await
        .expect("repair should succeed");

    assert!(report.committed);
    assert_eq!(report.validation.output_size, 100);

    let final_content = fs::read_to_string(baseline.join("config.txt")).unwrap();
    assert!(final_content.contains("fixed=true"));
}
