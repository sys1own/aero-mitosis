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
    Message(String),
}

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouterError::Io(e) => write!(f, "router I/O error: {e}"),
            RouterError::Virtualizer(e) => write!(f, "router virtualizer error: {e}"),
            RouterError::NoCandidates => write!(f, "no repair candidates were generated"),
            RouterError::Message(msg) => write!(f, "router error: {msg}"),
        }
    }
}

impl Error for RouterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            RouterError::Io(e) => Some(e),
            RouterError::Virtualizer(e) => Some(e),
            RouterError::NoCandidates | RouterError::Message(_) => None,
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
pub struct SelfHealingRouter {
    config: SelfHealingConfig,
    virtualizer: DefaultVirtualizer,
}

impl SelfHealingRouter {
    pub fn new(config: SelfHealingConfig) -> Self {
        Self {
            config,
            virtualizer: DefaultVirtualizer::new(),
        }
    }

    /// Run the fault recovery loop.
    ///
    /// `baseline_root` is the host directory that will be used as the immutable
    /// lower layer. If a patch is committed, the lower layer is updated in place.
    pub async fn repair(
        &self,
        baseline_root: &Path,
        failure: &CompilerFailure,
        graph: &StructuralCausalGraph,
        deltas: &HashMap<usize, FeatureVector>,
        baseline: &ValidationBaseline,
        generator: &dyn PatchGenerator,
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

        let mut last_report = None;
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

            last_report = Some(RepairReport {
                committed: false,
                penalty,
                patch,
                validation,
                final_message: format!(
                    "Patch attempt {} for root cause '{}' failed or exceeded penalty threshold (penalty={penalty:.2}).",
                    attempt + 1,
                    root_cause
                ),
            });

            // Fast 10ms discard loop to purge the sandbox overlay.
            self.virtualizer.teardown(&env).await?;
            purge_overlay(&env, Duration::from_millis(10)).await;
        }

        last_report.ok_or(RouterError::NoCandidates)
    }

    /// Compute the Multi-Objective Penalty Index for a validation result.
    pub fn penalty_index(
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
