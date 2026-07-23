//! Self-healing repair router for the autonomic CI engine.
//!
//! The router ties together causal diagnostics, the workspace virtualizer, and
//! a validation backend to run a closed fault-recovery loop:
//!
//! 1. Diagnose a `CompilerFailure` into a root-cause node.
//! 2. Synthesize one or more candidate patches.
//! 3. Mount an isolated overlay workspace for each candidate.
//! 4. Apply the patch, validate the result, and compute a Multi-Objective
//!    Penalty Index `P = w_1·Δτ + w_2·Δβ`.
//! 5. Commit the patch if validation succeeds and `P` is under threshold;
//!    otherwise purge the overlay in a fast discard loop.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use autonomic_ci_parser::ast::matrix_mapper::FeatureVector;
use autonomic_ci_parser::scm::ingestion::StructuralCausalGraph;
use autonomic_ci_virtualizer::virtualization::{
    DefaultVirtualizer, VirtualEnvConfig, VirtualizerError, WorkspaceVirtualizer,
};
use tokio::task;
use tokio::time::sleep;

use crate::diagnostics::causal_gradient::{CausalGradientAnalyzer, CompilerFailure};

/// A single file modification to be applied inside the sandboxed overlay.
#[derive(Debug, Clone)]
pub struct Patch {
    pub path: PathBuf,
    pub content: String,
}

/// Weights for the multi-objective penalty index.
#[derive(Debug, Clone, Copy)]
pub struct PenaltyWeights {
    pub w_time: f64,
    pub w_size: f64,
}

impl Default for PenaltyWeights {
    fn default() -> Self {
        Self {
            w_time: 1.0,
            w_size: 0.001,
        }
    }
}

/// Baseline measurements used to penalize a candidate patch.
#[derive(Debug, Clone, Copy)]
pub struct ValidationBaseline {
    pub time_ms: u64,
    pub output_size: u64,
}

/// Configuration controlling the self-healing loop.
#[derive(Debug, Clone, Copy)]
pub struct SelfHealingConfig {
    pub weights: PenaltyWeights,
    pub penalty_threshold: f64,
    pub max_retries: usize,
}

impl Default for SelfHealingConfig {
    fn default() -> Self {
        Self {
            weights: PenaltyWeights::default(),
            penalty_threshold: 1000.0,
            max_retries: 3,
        }
    }
}

/// Result of a single validation pass.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub success: bool,
    pub elapsed_ms: u64,
    pub output_size: u64,
    pub stderr: String,
}

/// Final report produced by a repair attempt.
#[derive(Debug, Clone)]
pub struct RepairReport {
    pub committed: bool,
    pub penalty: f64,
    pub patch: Patch,
    pub validation: ValidationResult,
    pub final_message: String,
}

/// Something that can validate a candidate workspace.
pub trait Validator: Send + Sync {
    fn validate(&self, root: &Path) -> ValidationResult;
}

/// Something that can synthesize candidate patches from a failure context.
pub trait PatchGenerator: Send + Sync {
    fn generate(
        &self,
        failure: &CompilerFailure,
        graph: &StructuralCausalGraph,
        deltas: &HashMap<usize, FeatureVector>,
    ) -> Vec<Patch>;
}

/// Errors that can occur during the self-healing loop.
#[derive(Debug)]
pub enum RouterError {
    Io(io::Error),
    Virtualizer(VirtualizerError),
    NoCandidates,
    ValidationFailed { reason: String, penalty: f64 },
    Message(String),
}

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouterError::Io(e) => write!(f, "router I/O error: {e}"),
            RouterError::Virtualizer(e) => write!(f, "router virtualizer error: {e}"),
            RouterError::NoCandidates => write!(f, "no repair candidates were generated"),
            RouterError::ValidationFailed { reason, penalty } => {
                write!(f, "validation failed (penalty={penalty:.2}): {reason}")
            }
            RouterError::Message(msg) => write!(f, "router error: {msg}"),
        }
    }
}

impl Error for RouterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            RouterError::Io(e) => Some(e),
            RouterError::Virtualizer(e) => Some(e),
            RouterError::NoCandidates
            | RouterError::ValidationFailed { .. }
            | RouterError::Message(_) => None,
        }
    }
}

impl From<io::Error> for RouterError {
    fn from(err: io::Error) -> Self {
        RouterError::Io(err)
    }
}

impl From<VirtualizerError> for RouterError {
    fn from(err: VirtualizerError) -> Self {
        RouterError::Virtualizer(err)
    }
}

/// Coordinates the fault recovery loop.
pub struct SelfHealingRouter<V = DefaultVirtualizer>
where
    V: WorkspaceVirtualizer,
{
    config: SelfHealingConfig,
    virtualizer: V,
}

impl SelfHealingRouter<DefaultVirtualizer> {
    /// Create a router using the platform default virtualizer.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use autonomic_ci_core::self_healing::router::{SelfHealingConfig, SelfHealingRouter};
    ///
    /// let config = SelfHealingConfig::default();
    /// let router = SelfHealingRouter::new(config);
    /// ```
    pub fn new(config: SelfHealingConfig) -> Self {
        Self {
            config,
            virtualizer: DefaultVirtualizer::new(),
        }
    }
}

impl<V> SelfHealingRouter<V>
where
    V: WorkspaceVirtualizer,
{
    /// Create a router with a custom virtualizer (useful for testing).
    pub fn with_virtualizer(config: SelfHealingConfig, virtualizer: V) -> Self {
        Self {
            config,
            virtualizer,
        }
    }

    /// Run the fault recovery loop.
    ///
    /// `baseline_root` is the host directory that will be used as the immutable
    /// lower layer. If a patch is committed, the lower layer is updated in place.
    #[allow(clippy::too_many_arguments)]
    pub async fn repair(
        &self,
        baseline_root: &Path,
        failure: &CompilerFailure,
        graph: &StructuralCausalGraph,
        deltas: &HashMap<usize, FeatureVector>,
        baseline: &ValidationBaseline,
        generator: Arc<dyn PatchGenerator>,
        validator: Arc<dyn Validator>,
    ) -> Result<RepairReport, RouterError> {
        let diagnostic = CausalGradientAnalyzer::diagnose(failure, graph, deltas);
        let candidates = generator.generate(failure, graph, deltas);

        if candidates.is_empty() {
            return Err(RouterError::NoCandidates);
        }

        let root_cause = diagnostic
            .root_cause_node
            .and_then(|id| graph.nodes.get(id))
            .map(|n| n.name.clone())
            .unwrap_or_else(|| "unknown".into());

        let mut last_error: Option<RouterError> = None;
        for (attempt, patch) in candidates
            .into_iter()
            .take(self.config.max_retries)
            .enumerate()
        {
            let env = make_env(baseline_root, attempt)?;
            self.virtualizer.initialize(&env).await?;
            self.virtualizer.mount(&env).await?;

            apply_patch(&env, &patch)?;
            self.virtualizer.synchronize_upper(&env).await?;

            let merged = env.merged_dir.clone();
            let v = Arc::clone(&validator);
            let validation = task::spawn_blocking(move || v.validate(&merged))
                .await
                .map_err(|e| RouterError::Message(e.to_string()))?;

            let penalty = compute_penalty(&validation, baseline, &self.config.weights);

            if validation.success && penalty <= self.config.penalty_threshold {
                self.virtualizer.commit(&env).await?;
                return Ok(RepairReport {
                    committed: true,
                    penalty,
                    patch,
                    validation,
                    final_message: format!(
                        "Patch accepted for root cause '{root_cause}' (penalty={penalty:.2} under threshold={:.2}). Commit applied.",
                        self.config.penalty_threshold
                    ),
                });
            }

            last_error = Some(RouterError::ValidationFailed {
                reason: validation.stderr.clone(),
                penalty,
            });

            // Fast 10ms discard loop to purge the sandbox overlay.
            self.virtualizer.teardown(&env).await?;
            purge_overlay(&env, Duration::from_millis(10)).await;
        }

        Err(last_error.unwrap_or(RouterError::NoCandidates))
    }

    /// Compute the Multi-Objective Penalty Index for a validation result.
    pub fn penalty_index(
        &self,
        result: &ValidationResult,
        baseline: &ValidationBaseline,
        weights: &PenaltyWeights,
    ) -> f64 {
        compute_penalty(result, baseline, weights)
    }
}

fn compute_penalty(
    result: &ValidationResult,
    baseline: &ValidationBaseline,
    weights: &PenaltyWeights,
) -> f64 {
    let delta_tau = (result.elapsed_ms as f64 - baseline.time_ms as f64).max(0.0);
    let delta_beta = (result.output_size as f64 - baseline.output_size as f64).max(0.0);
    weights.w_time * delta_tau + weights.w_size * delta_beta
}

static ATTEMPT_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn make_env(baseline_root: &Path, attempt: usize) -> io::Result<VirtualEnvConfig> {
    let counter = ATTEMPT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "aero_self_heal_{}_{}_{}_{}",
        std::process::id(),
        now,
        counter,
        attempt
    ));

    let lower = baseline_root.to_path_buf();
    let upper = base.join("upper");
    let merged = base.join("merged");
    let work = base.join("work");

    fs::create_dir_all(&upper)?;
    fs::create_dir_all(&merged)?;
    fs::create_dir_all(&work)?;

    Ok(VirtualEnvConfig {
        lower_dir: lower,
        upper_dir: upper,
        merged_dir: merged,
        work_dir: work,
    })
}

fn apply_patch(env: &VirtualEnvConfig, patch: &Patch) -> io::Result<()> {
    let target = env.upper_dir.join(&patch.path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(target, &patch.content)
}

async fn purge_overlay(env: &VirtualEnvConfig, budget: Duration) {
    let start = Instant::now();
    let dirs = [&env.upper_dir, &env.merged_dir, &env.work_dir];
    while start.elapsed() < budget {
        for dir in &dirs {
            if dir.exists() {
                let _ = fs::remove_dir_all(dir);
            }
        }
        if dirs.iter().all(|d| !d.exists()) {
            break;
        }
        sleep(Duration::from_millis(1)).await;
    }
}

/// A simple patch generator that always produces the same repair patch.
#[derive(Clone)]
pub struct StaticPatchGenerator {
    pub patches: Vec<Patch>,
}

impl PatchGenerator for StaticPatchGenerator {
    fn generate(
        &self,
        _failure: &CompilerFailure,
        _graph: &StructuralCausalGraph,
        _deltas: &HashMap<usize, FeatureVector>,
    ) -> Vec<Patch> {
        self.patches.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use autonomic_ci_parser::scm::ingestion::{NodeType, SCMNode, StructuralCausalGraph};
    use autonomic_ci_virtualizer::virtualization::CommitReport;

    struct AlwaysFailValidator;

    impl Validator for AlwaysFailValidator {
        fn validate(&self, _root: &Path) -> ValidationResult {
            ValidationResult {
                success: false,
                elapsed_ms: 50,
                output_size: 0,
                stderr: "always fails".into(),
            }
        }
    }

    struct AlwaysPassValidator;

    impl Validator for AlwaysPassValidator {
        fn validate(&self, _root: &Path) -> ValidationResult {
            ValidationResult {
                success: true,
                elapsed_ms: 100,
                output_size: 100,
                stderr: String::new(),
            }
        }
    }

    struct AlwaysExpensiveValidator;

    impl Validator for AlwaysExpensiveValidator {
        fn validate(&self, _root: &Path) -> ValidationResult {
            ValidationResult {
                success: true,
                elapsed_ms: 100,
                output_size: 200_000,
                stderr: String::new(),
            }
        }
    }

    struct PanicValidator;

    impl Validator for PanicValidator {
        fn validate(&self, _root: &Path) -> ValidationResult {
            panic!("validator panic");
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum FailingStep {
        Initialize,
        Mount,
        SynchronizeUpper,
        Commit,
        Teardown,
    }

    struct MockFailingVirtualizer {
        step: FailingStep,
    }

    #[allow(async_fn_in_trait)]
    impl WorkspaceVirtualizer for MockFailingVirtualizer {
        async fn initialize(&self, _config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
            if self.step == FailingStep::Initialize {
                Err(VirtualizerError::SystemFault("init failed".into()))
            } else {
                Ok(())
            }
        }

        async fn mount(&self, _config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
            if self.step == FailingStep::Mount {
                Err(VirtualizerError::SystemFault("mount failed".into()))
            } else {
                Ok(())
            }
        }

        async fn synchronize_upper(
            &self,
            _config: &VirtualEnvConfig,
        ) -> Result<(), VirtualizerError> {
            if self.step == FailingStep::SynchronizeUpper {
                Err(VirtualizerError::SystemFault("sync failed".into()))
            } else {
                Ok(())
            }
        }

        async fn commit(
            &self,
            _config: &VirtualEnvConfig,
        ) -> Result<CommitReport, VirtualizerError> {
            if self.step == FailingStep::Commit {
                Err(VirtualizerError::SystemFault("commit failed".into()))
            } else {
                Ok(CommitReport::default())
            }
        }

        async fn teardown(&self, _config: &VirtualEnvConfig) -> Result<(), VirtualizerError> {
            if self.step == FailingStep::Teardown {
                Err(VirtualizerError::SystemFault("teardown failed".into()))
            } else {
                Ok(())
            }
        }
    }

    fn make_graph_and_deltas() -> (StructuralCausalGraph, HashMap<usize, FeatureVector>) {
        let mut graph = StructuralCausalGraph::new();
        let node = graph.add_node(SCMNode {
            id: 0,
            name: "demo".into(),
            path: Default::default(),
            language: "rust".into(),
            node_type: NodeType::Package,
        });

        let mut deltas = HashMap::new();
        deltas.insert(
            node,
            FeatureVector {
                labels: vec!["namespace", "structural", "functional"],
                values: vec![0.0, 1.0, 0.0],
            },
        );
        (graph, deltas)
    }

    fn baseline_with(content: &str) -> (PathBuf, ValidationBaseline) {
        let base = std::env::temp_dir().join(format!(
            "aero_router_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config.txt"), content).unwrap();
        (
            base,
            ValidationBaseline {
                time_ms: 100,
                output_size: 100,
            },
        )
    }

    fn failure() -> CompilerFailure {
        CompilerFailure {
            node_id: Some(0),
            message: "fail".into(),
            exit_code: Some(1),
        }
    }

    #[tokio::test]
    async fn no_candidates_returns_error() {
        let (graph, deltas) = make_graph_and_deltas();
        let (baseline, metrics) = baseline_with("unrecognized\n");
        let router = SelfHealingRouter::new(SelfHealingConfig::default());

        let result = router
            .repair(
                &baseline,
                &failure(),
                &graph,
                &deltas,
                &metrics,
                Arc::new(StaticPatchGenerator { patches: vec![] }),
                Arc::new(AlwaysFailValidator),
            )
            .await;

        assert!(matches!(result, Err(RouterError::NoCandidates)));
    }

    #[tokio::test]
    async fn all_candidates_fail_validation_after_retries() {
        let (graph, deltas) = make_graph_and_deltas();
        let (baseline, metrics) = baseline_with("broken=true\n");

        let generator = Arc::new(StaticPatchGenerator {
            patches: vec![
                Patch {
                    path: PathBuf::from("config.txt"),
                    content: "broken=true\n".into(),
                },
                Patch {
                    path: PathBuf::from("config.txt"),
                    content: "broken=true\n".into(),
                },
                Patch {
                    path: PathBuf::from("config.txt"),
                    content: "broken=true\n".into(),
                },
            ],
        });

        let router = SelfHealingRouter::new(SelfHealingConfig::default());
        let result = router
            .repair(
                &baseline,
                &failure(),
                &graph,
                &deltas,
                &metrics,
                generator,
                Arc::new(AlwaysFailValidator),
            )
            .await;

        assert!(matches!(result, Err(RouterError::ValidationFailed { .. })));
    }

    #[tokio::test]
    async fn all_candidates_exceed_penalty_threshold() {
        let (graph, deltas) = make_graph_and_deltas();
        let (baseline, metrics) = baseline_with("any\n");

        let generator = Arc::new(StaticPatchGenerator {
            patches: vec![
                Patch {
                    path: PathBuf::from("config.txt"),
                    content: "expensive=true\n".into(),
                },
                Patch {
                    path: PathBuf::from("config.txt"),
                    content: "expensive=true\n".into(),
                },
            ],
        });

        let config = SelfHealingConfig {
            weights: PenaltyWeights::default(),
            penalty_threshold: 100.0,
            max_retries: 3,
        };
        let router = SelfHealingRouter::new(config);

        let result = router
            .repair(
                &baseline,
                &failure(),
                &graph,
                &deltas,
                &metrics,
                generator,
                Arc::new(AlwaysExpensiveValidator),
            )
            .await;

        assert!(matches!(result, Err(RouterError::ValidationFailed { .. })));
    }

    #[tokio::test]
    async fn validation_panic_is_caught() {
        let (graph, deltas) = make_graph_and_deltas();
        let (baseline, metrics) = baseline_with("any\n");

        let generator = Arc::new(StaticPatchGenerator {
            patches: vec![Patch {
                path: PathBuf::from("config.txt"),
                content: "fixed=true\n".into(),
            }],
        });

        let router = SelfHealingRouter::new(SelfHealingConfig::default());
        let result = router
            .repair(
                &baseline,
                &failure(),
                &graph,
                &deltas,
                &metrics,
                generator,
                Arc::new(PanicValidator),
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn virtualizer_fails_at_each_step() {
        let (graph, deltas) = make_graph_and_deltas();

        for step in [
            FailingStep::Initialize,
            FailingStep::Mount,
            FailingStep::SynchronizeUpper,
            FailingStep::Commit,
            FailingStep::Teardown,
        ] {
            let (baseline, metrics) = baseline_with("any\n");
            let virtualizer = MockFailingVirtualizer { step };
            let router =
                SelfHealingRouter::with_virtualizer(SelfHealingConfig::default(), virtualizer);

            let generator = Arc::new(StaticPatchGenerator {
                patches: vec![Patch {
                    path: PathBuf::from("config.txt"),
                    content: "fixed=true\n".into(),
                }],
            });

            let result = router
                .repair(
                    &baseline,
                    &failure(),
                    &graph,
                    &deltas,
                    &metrics,
                    generator,
                    if step == FailingStep::Commit {
                        Arc::new(AlwaysPassValidator)
                    } else {
                        Arc::new(AlwaysFailValidator)
                    },
                )
                .await;

            assert!(result.is_err(), "step {:?} did not produce an error", step);
        }
    }

    #[tokio::test]
    async fn penalty_index_at_and_above_threshold() {
        let router = SelfHealingRouter::new(SelfHealingConfig::default());

        let at = ValidationResult {
            success: true,
            elapsed_ms: 200,
            output_size: 100,
            stderr: String::new(),
        };
        let baseline = ValidationBaseline {
            time_ms: 100,
            output_size: 100,
        };

        assert_eq!(
            router.penalty_index(&at, &baseline, &PenaltyWeights::default()),
            100.0
        );

        let above = ValidationResult {
            success: true,
            elapsed_ms: 201,
            output_size: 100,
            stderr: String::new(),
        };
        assert!(router.penalty_index(&above, &baseline, &PenaltyWeights::default()) > 100.0);
    }

    #[test]
    fn patch_application_creates_nested_files_and_symlinks() {
        let base = std::env::temp_dir().join(format!("aero_patch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let env = VirtualEnvConfig {
            lower_dir: base.join("lower"),
            upper_dir: base.join("upper"),
            merged_dir: base.join("merged"),
            work_dir: base.join("work"),
        };
        std::fs::create_dir_all(&env.upper_dir).unwrap();
        std::fs::create_dir_all(&env.lower_dir).unwrap();

        // Nested file.
        apply_patch(
            &env,
            &Patch {
                path: PathBuf::from("a/b/c.txt"),
                content: "nested\n".into(),
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(env.upper_dir.join("a/b/c.txt")).unwrap(),
            "nested\n"
        );

        // Existing file is overwritten.
        std::fs::write(env.upper_dir.join("existing.txt"), "old\n").unwrap();
        apply_patch(
            &env,
            &Patch {
                path: PathBuf::from("existing.txt"),
                content: "new\n".into(),
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(env.upper_dir.join("existing.txt")).unwrap(),
            "new\n"
        );

        // Symlink as parent directory: the write follows the link on Unix.
        #[cfg(unix)]
        {
            std::fs::create_dir_all(env.upper_dir.join("real_dir")).unwrap();
            std::os::unix::fs::symlink(
                &env.upper_dir.join("real_dir"),
                &env.upper_dir.join("link"),
            )
            .unwrap();
            apply_patch(
                &env,
                &Patch {
                    path: PathBuf::from("link/sub/file.txt"),
                    content: "through symlink\n".into(),
                },
            )
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(env.upper_dir.join("real_dir/sub/file.txt")).unwrap(),
                "through symlink\n"
            );
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn large_output_penalty_is_calculated() {
        let result = ValidationResult {
            success: true,
            elapsed_ms: 100,
            output_size: 1_000_000,
            stderr: String::new(),
        };
        let baseline = ValidationBaseline {
            time_ms: 100,
            output_size: 0,
        };

        let penalty = compute_penalty(&result, &baseline, &PenaltyWeights::default());
        assert_eq!(penalty, 1000.0);
    }

    #[tokio::test]
    async fn concurrent_repairs_use_unique_environments() {
        let config = SelfHealingConfig {
            weights: PenaltyWeights::default(),
            penalty_threshold: 1000.0,
            max_retries: 1,
        };

        let before = ATTEMPT_COUNTER.load(Ordering::Relaxed);
        let mut handles = Vec::new();

        for i in 0..10usize {
            let handle = tokio::spawn(async move {
                let (baseline, metrics) = baseline_with(&format!("base{i}\n"));
                let router = SelfHealingRouter::new(config);
                let mut graph = StructuralCausalGraph::new();
                let node = graph.add_node(SCMNode {
                    id: 0,
                    name: "demo".into(),
                    path: Default::default(),
                    language: "rust".into(),
                    node_type: NodeType::Package,
                });

                let mut deltas = HashMap::new();
                deltas.insert(
                    node,
                    FeatureVector {
                        labels: vec!["namespace", "structural", "functional"],
                        values: vec![0.0, 1.0, 0.0],
                    },
                );

                let generator = Arc::new(StaticPatchGenerator {
                    patches: vec![Patch {
                        path: PathBuf::from("config.txt"),
                        content: "fixed=true\n".into(),
                    }],
                });

                let report = router
                    .repair(
                        &baseline,
                        &failure(),
                        &graph,
                        &deltas,
                        &metrics,
                        generator,
                        Arc::new(AlwaysPassValidator),
                    )
                    .await;

                assert!(report.unwrap().committed);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let after = ATTEMPT_COUNTER.load(Ordering::Relaxed);
        assert!(after - before >= 10);
    }
}
