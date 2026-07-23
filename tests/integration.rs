//! End-to-end integration tests that exercise the full aero-mitosis pipeline.
//!
//! These tests build and run real Rust projects inside the virtualizer sandbox.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use autonomic_ci_core::diagnostics::causal_gradient::CompilerFailure;
use autonomic_ci_core::self_healing::router::{
    Patch, PatchGenerator, PenaltyWeights, SelfHealingConfig, SelfHealingRouter,
    StaticPatchGenerator, ValidationBaseline, ValidationResult, Validator,
};
use autonomic_ci_parser::ast::matrix_mapper::FeatureVector;
use autonomic_ci_parser::scm::ingestion::{NodeType, SCMNode, StructuralCausalGraph};
use autonomic_ci_virtualizer::virtualization::{
    DefaultVirtualizer, VirtualEnvConfig, WorkspaceVirtualizer,
};

fn temp_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn write_rust_project(base: &Path, lib_rs: &str) {
    fs::create_dir_all(base.join("src")).unwrap();
    fs::write(
        base.join("Cargo.toml"),
        r#"
[package]
name = "sandbox"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    fs::write(base.join("src/lib.rs"), lib_rs).unwrap();
    fs::write(
        base.join("src/main.rs"),
        "fn main() { println!(\"{}\", sandbox::answer()); }\n",
    )
    .unwrap();
}

#[tokio::test]
async fn cargo_build_succeeds_inside_sandbox() {
    let base = temp_dir("aero_integration_build");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();

    let project = base.join("project");
    fs::create_dir_all(&project).unwrap();

    write_rust_project(
        &project,
        r#"
pub fn answer() -> i32 { 42 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn it_works() { assert_eq!(answer(), 42); }
}
"#,
    );

    let config = VirtualEnvConfig {
        lower_dir: project.clone(),
        upper_dir: base.join("upper"),
        merged_dir: base.join("merged"),
        work_dir: base.join("work"),
    };

    let virtualizer = DefaultVirtualizer::new();
    virtualizer.initialize(&config).await.unwrap();
    virtualizer.mount(&config).await.unwrap();

    let status = Command::new("cargo")
        .arg("build")
        .arg("--offline")
        .current_dir(&config.merged_dir)
        .status()
        .expect("cargo should be installed");

    assert!(status.success(), "cargo build failed in sandbox");
    let bin_name = if cfg!(windows) {
        "sandbox.exe"
    } else {
        "sandbox"
    };
    assert!(config
        .merged_dir
        .join("target/debug")
        .join(bin_name)
        .exists());

    virtualizer.teardown(&config).await.unwrap();
    let _ = fs::remove_dir_all(&base);
}

struct CargoTestValidator;

impl Validator for CargoTestValidator {
    fn validate(&self, root: &Path) -> ValidationResult {
        let status = Command::new("cargo")
            .arg("test")
            .arg("--offline")
            .current_dir(root)
            .status();

        match status {
            Ok(s) if s.success() => ValidationResult {
                success: true,
                elapsed_ms: 0,
                output_size: 0,
                stderr: String::new(),
            },
            _ => ValidationResult {
                success: false,
                elapsed_ms: 0,
                output_size: 0,
                stderr: "cargo test failed".into(),
            },
        }
    }
}

#[tokio::test]
async fn self_healing_router_fixes_failing_test() {
    let base = temp_dir("aero_integration_router");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();

    write_rust_project(
        &base,
        r#"
pub fn answer() -> i32 { 41 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn it_works() { assert_eq!(answer(), 42); }
}
"#,
    );

    let mut graph = StructuralCausalGraph::new();
    let node = graph.add_node(SCMNode {
        id: 0,
        name: "sandbox".into(),
        path: Default::default(),
        language: "rust".into(),
        node_type: NodeType::Package,
    });

    let mut deltas = std::collections::HashMap::new();
    deltas.insert(
        node,
        FeatureVector {
            labels: vec!["namespace", "structural", "functional"],
            values: vec![0.0, 1.0, 0.0],
        },
    );

    let failure = CompilerFailure {
        node_id: Some(node),
        message: "test failure".into(),
        exit_code: Some(101),
    };

    let generator: std::sync::Arc<dyn PatchGenerator> = std::sync::Arc::new(StaticPatchGenerator {
        patches: vec![Patch {
            path: PathBuf::from("src/lib.rs"),
            content: r#"
pub fn answer() -> i32 { 42 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn it_works() { assert_eq!(answer(), 42); }
}
"#
            .into(),
        }],
    });

    let config = SelfHealingConfig {
        weights: PenaltyWeights::default(),
        penalty_threshold: 1000.0,
        max_retries: 1,
    };
    let router = SelfHealingRouter::new(config);
    let baseline = ValidationBaseline {
        time_ms: 100,
        output_size: 100,
    };

    let report = router
        .repair(
            &base,
            &failure,
            &graph,
            &deltas,
            &baseline,
            generator,
            std::sync::Arc::new(CargoTestValidator),
        )
        .await
        .expect("router should fix the test");

    assert!(report.committed);

    let fixed_lib = fs::read_to_string(base.join("src/lib.rs")).unwrap();
    assert!(fixed_lib.contains("42"));

    let _ = fs::remove_dir_all(&base);
}
