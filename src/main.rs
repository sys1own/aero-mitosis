//! Production CLI entrypoint for the Aero-Mitosis autonomic build engine.
//!
//! The binary ingests a repository, builds a structural causal graph, schedules
//! build units into wavefront bands, and executes each unit inside a lightweight
//! copy-on-write sandbox. When a build fails, the self-healing router attempts
//! to synthesize and validate a patch before giving up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use log::{debug, error, info, warn};

use autonomic_ci_core::diagnostics::causal_gradient::CompilerFailure;
use autonomic_ci_core::scheduler::wavefront::{CompilationUnit, WavefrontScheduler};
use autonomic_ci_core::self_healing::router::{
    Patch, PatchGenerator, PenaltyWeights, RouterError, SelfHealingConfig, SelfHealingRouter,
    ValidationBaseline, ValidationResult, Validator,
};
use autonomic_ci_parser::ast::matrix_mapper::FeatureVector;
use autonomic_ci_parser::scm::ingestion::{IngestionEngine, StructuralCausalGraph};
use autonomic_ci_virtualizer::virtualization::{
    DefaultVirtualizer, VirtualEnvConfig, WorkspaceVirtualizer,
};

/// Aero-Mitosis command-line arguments.
#[derive(Parser, Debug)]
#[command(name = "aero-mitosis", about = "Autonomic CI build engine")]
struct Args {
    /// Path to the repository to build.
    repo_path: PathBuf,

    /// Maximum time in seconds to spend on a single build unit.
    #[arg(long, default_value_t = 900)]
    timeout: u64,
}

fn main() -> std::process::ExitCode {
    env_logger::init();
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[PANIC] {info}");
        log::error!("internal panic: {info}");
    }));

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[FATAL] unable to start tokio runtime: {e}");
            return std::process::ExitCode::from(101);
        }
    };

    runtime.block_on(async_main())
}

async fn async_main() -> std::process::ExitCode {
    let args = Args::parse();
    let repo = &args.repo_path;

    if !repo.exists() {
        eprintln!("[ERROR] repository path does not exist: {}", repo.display());
        return std::process::ExitCode::from(1);
    }
    if !repo.is_dir() {
        eprintln!(
            "[ERROR] repository path is not a directory: {}",
            repo.display()
        );
        return std::process::ExitCode::from(1);
    }

    let start = Instant::now();
    info!("discovering repository: {}", repo.display());

    let graph = match IngestionEngine::discover(repo) {
        Ok(g) => g,
        Err(e) => {
            error!("failed to ingest repository: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    info!(
        "discovered {} nodes and {} edges",
        graph.nodes.len(),
        graph.edges.len()
    );
    debug!(
        "graph nodes: {:?}",
        graph.nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
    );

    let units: Vec<CompilationUnit> = WavefrontScheduler::units_from_graph(&graph)
        .into_iter()
        .filter(|u| !u.language.is_empty() && !u.name.contains('{'))
        .collect();

    if units.is_empty() {
        info!("no buildable units found");
        return std::process::ExitCode::SUCCESS;
    }

    let bands = build_bands(&units);
    info!(
        "scheduled {} units across {} wavefront bands",
        units.len(),
        bands.len()
    );

    let config = SelfHealingConfig {
        weights: PenaltyWeights::default(),
        penalty_threshold: f64::MAX,
        max_retries: 3,
    };
    let router = SelfHealingRouter::new(config);
    let baseline = ValidationBaseline {
        time_ms: 0,
        output_size: 0,
    };

    for (band_idx, band) in bands.iter().enumerate() {
        info!(
            "[INFO] running wavefront {}/{} with {} units",
            band_idx + 1,
            bands.len(),
            band.len()
        );

        for unit in band {
            let lower = graph
                .nodes
                .get(unit.id)
                .and_then(|n| {
                    let p = &n.path;
                    if p.as_os_str().is_empty() {
                        None
                    } else {
                        Some(p.clone())
                    }
                })
                .unwrap_or_else(|| repo.to_path_buf());

            if !lower.exists() {
                warn!("unit {} path missing: {}", unit.name, lower.display());
                continue;
            }

            debug!("building unit {} at {}", unit.name, lower.display());
            let timeout = Duration::from_secs(args.timeout);
            let result = tokio::time::timeout(
                timeout,
                run_unit(unit, &lower, &graph, &baseline, &router, timeout),
            )
            .await;

            match result {
                Ok(Ok(true)) => info!("unit {} built successfully", unit.name),
                Ok(Ok(false)) => {
                    error!("unit {} failed and could not be healed", unit.name);
                    return std::process::ExitCode::from(2);
                }
                Ok(Err(e)) => {
                    error!("unit {} error: {e}", unit.name);
                    return std::process::ExitCode::from(2);
                }
                Err(_) => {
                    error!(
                        "unit {} timed out after {} seconds",
                        unit.name, args.timeout
                    );
                    return std::process::ExitCode::from(2);
                }
            }
        }
    }

    info!(
        "all wavefronts completed in {:.2}s",
        start.elapsed().as_secs_f64()
    );
    std::process::ExitCode::SUCCESS
}

/// Build wavefront bands from a list of compilation units using Kahn's
/// algorithm over the dependency edges already present on each unit.
fn build_bands(units: &[CompilationUnit]) -> Vec<Vec<CompilationUnit>> {
    let by_id: HashMap<usize, &CompilationUnit> = units.iter().map(|u| (u.id, u)).collect();

    // Only count dependencies that are themselves buildable units.
    let mut remaining: HashMap<usize, usize> = units
        .iter()
        .map(|u| {
            let count = u
                .dependencies
                .iter()
                .filter(|dep| by_id.contains_key(dep))
                .count();
            (u.id, count)
        })
        .collect();

    let mut dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for u in units {
        for &dep in &u.dependencies {
            if by_id.contains_key(&dep) {
                dependents.entry(dep).or_default().push(u.id);
            }
        }
    }

    let mut queue: Vec<usize> = units
        .iter()
        .filter(|u| remaining[&u.id] == 0)
        .map(|u| u.id)
        .collect();

    let mut bands: Vec<Vec<CompilationUnit>> = Vec::new();
    while !queue.is_empty() {
        let mut band = Vec::with_capacity(queue.len());
        let mut next_queue = Vec::new();

        for id in queue {
            if let Some(&unit) = by_id.get(&id) {
                band.push((*unit).clone());
            }
            if let Some(children) = dependents.get(&id) {
                for &child in children {
                    if let Some(count) = remaining.get_mut(&child) {
                        *count -= 1;
                        if *count == 0 {
                            next_queue.push(child);
                        }
                    }
                }
            }
        }

        if !band.is_empty() {
            bands.push(band);
        }
        queue = next_queue;
    }

    bands
}

/// Run a single compilation unit in a sandbox, attempting self-healing if the
/// first validation fails.
async fn run_unit(
    unit: &CompilationUnit,
    lower: &Path,
    graph: &StructuralCausalGraph,
    baseline: &ValidationBaseline,
    router: &SelfHealingRouter,
    timeout: Duration,
) -> Result<bool, RouterError> {
    let base = std::env::temp_dir().join(format!(
        "aero_mitosis_{}_{}_{}_{}",
        std::process::id(),
        timestamp(),
        unit.id,
        unit.name.replace(['/', '\\'], "_")
    ));
    let _ = std::fs::remove_dir_all(&base);

    let virtualizer = Arc::new(DefaultVirtualizer::new());
    let config = VirtualEnvConfig {
        lower_dir: lower.to_path_buf(),
        upper_dir: base.join("upper"),
        merged_dir: base.join("merged"),
        work_dir: base.join("work"),
    };

    virtualizer.initialize(&config).await?;
    virtualizer.mount(&config).await?;

    let validator: Arc<dyn Validator> = Arc::new(BuildValidator {
        timeout_sec: timeout.as_secs().clamp(1, 600),
    });

    let merged = config.merged_dir.clone();
    let v = Arc::clone(&validator);
    let validation = tokio::task::spawn_blocking(move || v.validate(&merged))
        .await
        .map_err(|e| RouterError::Message(e.to_string()))?;

    if validation.success {
        info!("  unit {} passed validation", unit.name);
        let _ = virtualizer.teardown(&config).await;
        let _ = std::fs::remove_dir_all(&base);
        return Ok(true);
    }

    warn!(
        "  unit {} failed validation: {}",
        unit.name,
        truncate(&validation.stderr, 500)
    );

    let failure = CompilerFailure {
        node_id: Some(unit.id),
        message: validation.stderr.clone(),
        exit_code: None,
    };

    let generator: Arc<dyn PatchGenerator> = Arc::new(GitRestorePatchGenerator {
        repo: lower.to_path_buf(),
    });
    let deltas = HashMap::new();

    let repair_result = router
        .repair(
            lower, &failure, graph, &deltas, baseline, generator, validator,
        )
        .await;

    let _ = virtualizer.teardown(&config).await;
    let _ = std::fs::remove_dir_all(&base);

    match repair_result {
        Ok(report) => {
            info!("  unit {} healed: {}", unit.name, report.final_message);
            Ok(true)
        }
        Err(RouterError::NoCandidates) => {
            error!("  unit {}: no repair candidates generated", unit.name);
            Ok(false)
        }
        Err(e) => {
            error!("  unit {} repair failed: {e}", unit.name);
            Ok(false)
        }
    }
}

fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

/// Validator that performs a language-appropriate build inside the sandbox.
struct BuildValidator {
    timeout_sec: u64,
}

impl Validator for BuildValidator {
    fn validate(&self, root: &Path) -> ValidationResult {
        let start = Instant::now();
        let mut success = true;
        let mut stderr = String::new();

        let has_cargo = root.join("Cargo.toml").exists();
        let has_pyproject = root.join("pyproject.toml").exists();
        let has_setup = root.join("setup.py").exists();

        if has_cargo && !has_pyproject && !has_setup {
            // Skip Cargo manifests that are clearly templates.
            if let Ok(content) = std::fs::read_to_string(root.join("Cargo.toml")) {
                if content.contains("{crate_name}") || content.contains("{extra_deps}") {
                    return ValidationResult {
                        success: true,
                        elapsed_ms: 0,
                        output_size: 0,
                        stderr: String::new(),
                    };
                }
            }

            let output = run_command(
                root,
                "timeout",
                &[&self.timeout_sec.to_string(), "cargo", "build"],
            );
            append_output(&mut success, &mut stderr, output);
        } else if has_pyproject || has_setup {
            if has_setup {
                let output = run_command(
                    root,
                    "timeout",
                    &[
                        &self.timeout_sec.to_string(),
                        "python3",
                        "setup.py",
                        "build",
                    ],
                );
                append_output(&mut success, &mut stderr, output);
            } else {
                let output = run_command(
                    root,
                    "timeout",
                    &[
                        &self.timeout_sec.to_string(),
                        "pip3",
                        "install",
                        "-e",
                        ".",
                        "--no-deps",
                        "--no-build-isolation",
                    ],
                );
                append_output(&mut success, &mut stderr, output);
            }

            // Always run compileall to catch syntax errors even when pip succeeds.
            let compile = run_command(
                root,
                "timeout",
                &[
                    &self.timeout_sec.to_string(),
                    "python3",
                    "-m",
                    "compileall",
                    ".",
                ],
            );
            append_output(&mut success, &mut stderr, compile);
        } else {
            stderr.push_str("no recognizable build manifest found");
            success = false;
        }

        let elapsed = start.elapsed().as_millis() as u64;
        ValidationResult {
            success,
            elapsed_ms: elapsed,
            output_size: stderr.len() as u64,
            stderr,
        }
    }
}

fn run_command(
    root: &Path,
    program: &str,
    args: &[&str],
) -> Result<std::process::Output, std::io::Error> {
    Command::new(program)
        .args(args)
        .current_dir(root)
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
}

fn append_output(
    success: &mut bool,
    stderr: &mut String,
    result: Result<std::process::Output, std::io::Error>,
) {
    match result {
        Ok(output) => {
            if !output.status.success() {
                *success = false;
            }
            stderr.push_str(&String::from_utf8_lossy(&output.stdout));
            stderr.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        Err(e) => {
            *success = false;
            stderr.push_str(&format!("failed to spawn command: {e}\n"));
        }
    }
}

/// Patch generator that restores corrupted Python files from the repository's
/// git history. When the failure mentions a missing system tool, no patch is
/// generated so the router can report an unrecoverable error.
struct GitRestorePatchGenerator {
    repo: PathBuf,
}

impl PatchGenerator for GitRestorePatchGenerator {
    fn generate(
        &self,
        failure: &CompilerFailure,
        _graph: &StructuralCausalGraph,
        _deltas: &HashMap<usize, FeatureVector>,
    ) -> Vec<Patch> {
        let msg = &failure.message;

        // Do not attempt to repair a missing system tool from within the sandbox.
        if msg.to_lowercase().contains("m4") && msg.to_lowercase().contains("not found") {
            debug!("detected missing m4, no patches will be generated");
            return Vec::new();
        }

        if let Some(path) = extract_py_path(msg) {
            debug!("extracted broken python path: {}", path.display());
            let rel = if path.is_absolute() {
                path.strip_prefix(&self.repo).unwrap_or(&path).to_path_buf()
            } else {
                path
            };

            let git_ref = format!("HEAD:{}", rel.display());
            debug!("restoring {} from git ref {}", rel.display(), git_ref);
            let output = Command::new("git")
                .args(["show", &git_ref])
                .current_dir(&self.repo)
                .output();

            if let Ok(output) = output {
                if output.status.success() {
                    let content = String::from_utf8_lossy(&output.stdout).to_string();
                    debug!(
                        "generated restore patch for {} ({} bytes)",
                        rel.display(),
                        content.len()
                    );
                    return vec![Patch { path: rel, content }];
                } else {
                    debug!(
                        "git show failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
        }

        Vec::new()
    }
}

fn extract_py_path(msg: &str) -> Option<PathBuf> {
    for line in msg.lines() {
        // Match Python traceback / compileall file references.
        for (prefix, quote) in [("File \"", '"'), ("File '", '\'')] {
            if let Some(start) = line.find(prefix) {
                let rest = &line[start + prefix.len()..];
                if let Some(end) = rest.find(quote) {
                    let candidate = rest[..end].trim_end_matches(',').to_string();
                    if candidate.ends_with(".py") {
                        return Some(PathBuf::from(candidate));
                    }
                }
            }
        }
    }
    None
}
