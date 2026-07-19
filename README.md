# Aero-Mitosis 🧬

**Autonomic CI Engine & Self-Healing Build Orchestrator**

Welcome to **Aero-Mitosis**, a high-performance build orchestration engine designed to treat your software compilation process as a dynamic, self-healing system.

Standard CI tools are passive—they break when things go wrong, leaving you to clean up the mess. Aero-Mitosis is different. It’s an autonomic daemon that sits in your workspace, monitors your builds in real-time, and—when a failure is detected—automatically diagnoses the root cause and repairs your configuration so you can get back to coding.

## Why Aero-Mitosis?

* **Self-Healing Loops**: When a build fails due to configuration drift or environmental mismatches, our `SelfHealingRouter` creates an isolated sandbox, applies potential fixes, and commits the solution only after verifying it passes all tests.


* **Causal Inference**: Instead of guessing why a build failed, the engine calculates a **causal gradient** to mathematically pinpoint the exact dependency or flag modification that caused the failure.


* **Zero-Cost Virtualization**: We use native OS primitives (like `clonefile` on macOS or ProjFS on Windows) to create lightweight, "copy-on-write" sandbox overlays. You get complete build isolation without the massive overhead of full containers.


* **Wavefront Scheduling**: Our work-stealing scheduler breaks your build into parallel "wavefronts," maximizing CPU utilization while respecting dependency boundaries.



## The Tech Stack

Built as a high-performance, cross-platform Rust workspace:

* **`autonomic_ci_core`**: The brain of the operation, containing the work-stealing scheduler, the causal gradient diagnostic engine, and the repair router.


* **`autonomic_ci_parser`**: Uses **Tree-sitter** to ingest your project's source code and manifests, mapping them into an actionable, structural dependency graph.


* **`autonomic_ci_virtualizer`**: The OS-agnostic layer that manages sandboxed file isolation, providing a unified interface across Linux, macOS, and Windows.



## Getting Started

1. **Clone & Build**:
```bash
git clone https://github.com/sys1own/aero-mitosis.git
cd aero-mitosis
cargo build --workspace

```


2. **Run the Verification Suite**:
We’ve included an extensive suite of integration tests that simulate build failures, test the causal diagnostic engine, and verify the self-healing commit process.


```bash
cargo test --workspace

```
