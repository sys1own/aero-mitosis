# Aero-Mitosis 🧬

**Autonomic CI Engine & Self-Healing Build Orchestrator**

Welcome to **Aero-Mitosis**, a high-performance build orchestration engine designed to treat your software compilation process as a dynamic, self-healing system.

Aero-Mitosis is an autonomic daemon that sits in your workspace, monitors your builds in real-time, diagnoses the root cause for any failure, and repairs your configuration so you can get right back to coding.

---

## Why Aero-Mitosis?

* **Self-Healing Loops**: When a build fails due to configuration drift or environmental mismatches, our `SelfHealingRouter` creates an isolated sandbox, applies potential fixes, and commits the solution only after verifying it passes all tests.


* **Causal Inference**: Instead of guessing why a build failed, the engine calculates a **causal gradient** to mathematically pinpoint the exact dependency or flag modification that caused the failure.


* **Zero-Cost Virtualization**: We use native OS primitives (like `clonefile` on macOS or ProjFS on Windows) to create lightweight, "copy-on-write" sandbox overlays. You get complete build isolation without the massive overhead of full containers.


* **Wavefront Scheduling**: Our work-stealing scheduler breaks your build into parallel "wavefronts," maximizing CPU utilization while respecting dependency boundaries.



---

## System Architecture

The workspace is split into three core crates alongside a primary execution binary, giving you modular power with zero monolithic fluff:

```text
aero-mitosis/
├── Cargo.toml
├── src/
│   └── main.rs                     # Primary binary launcher[cite: 1]
└── crates/
    ├── autonomic_ci_core/          # Core scheduling, diagnostics & self-healing[cite: 1]
    ├── autonomic_ci_parser/        # AST analysis & SCM ingestion[cite: 1]
    └── autonomic_ci_virtualizer/   # Cross-platform filesystem sandboxing[cite: 1]

```

### 1. `autonomic_ci_core`

The brain of the operation. It houses the intelligence required to schedule builds, calculate diagnostic gradients, and execute self-healing loops:

* **Scheduler Module (`scheduler/`)**: Features **wavefront scheduling** (`wavefront.rs`) and **work-stealing tasks** (`work_stealing.rs`) to saturate CPU cores while maintaining strict dependency graph order.


* **Diagnostics Engine (`diagnostics/`)**: Includes **causal gradient analysis** (`causal_gradient.rs`) for failure tracking and an **FFI normalizer** (`ffi_normalizer.rs`) for safe cross-language boundaries.


* **Self-Healing Router (`self_healing/`)**: Houses the autonomous router (`router.rs`) to isolate, fix, and re-verify broken build state candidates.


* **Verification Suite (`tests/end_to_end.rs`)**: An extensive end-to-end integration test suite simulating real build failures, testing causal diagnostics, and confirming self-healing commits.



### 2. `autonomic_ci_parser`

Responsible for analyzing and digesting project structures and source control information:

* **AST Analysis (`ast/`)**: Leverages **Tree-sitter** to ingest source code and manifests, utilizing matrix mapping (`matrix_mapper.rs`) and AST query engines (`queries.rs`) to form a structural dependency graph.


* **SCM Ingestion (`scm/`)**: Ingests repository state (`ingestion.rs`) to evaluate incoming commits, branches, and patch matrices.



### 3. `autonomic_ci_virtualizer`

The OS-agnostic virtualization layer that manages lightweight sandboxed file isolation:

* **macOS Engine**: High-speed APFS snapshotting and cloning (`macos_apfs.rs`) paired with `FSEvents` filesystem monitoring (`macos_fsevents.rs`).


* **Windows Engine**: Native Projected File System (`windows_projfs.rs`) integration alongside automated filesystem fallback routines (`windows_fallback.rs`).


* **Unified Abstraction Layer**: Engine coordination (`engine.rs`) and common traits (`traits.rs`) providing a clean interface across Linux, macOS, and Windows.



---

## Project Directory Map

Here is a quick look at how the code is laid out across the workspace:

```text
.
├── Cargo.toml
├── README.md
├── src/
│   └── main.rs
└── crates/
    ├── autonomic_ci_core/
    │   ├── Cargo.toml
    │   ├── src/
    │   │   ├── lib.rs
    │   │   ├── diagnostics/
    │   │   │   ├── mod.rs
    │   │   │   ├── causal_gradient.rs
    │   │   │   └── ffi_normalizer.rs
    │   │   ├── scheduler/
    │   │   │   ├── mod.rs
    │   │   │   ├── wavefront.rs
    │   │   │   └── work_stealing.rs
    │   │   └── self_healing/
    │   │       ├── mod.rs
    │   │       └── router.rs
    │   └── tests/
    │       └── end_to_end.rs
    ├── autonomic_ci_parser/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── ast/
    │       │   ├── mod.rs
    │       │   ├── matrix_mapper.rs
    │       │   └── queries.rs
    │       └── scm/
    │           ├── mod.rs
    │           └── ingestion.rs
    └── autonomic_ci_virtualizer/
        ├── Cargo.toml
        └── src/
            ├── lib.rs
            └── virtualization/
                ├── mod.rs
                ├── engine.rs
                ├── traits.rs
                ├── macos_apfs.rs
                ├── macos_fsevents.rs
                ├── windows_projfs.rs
                └── windows_fallback.rs

```

---

## Getting Started

### Prerequisites

* **Rust Toolchain**: Make sure you have a modern Rust toolchain installed.


* **Platform Dependencies**:
* *macOS*: Uses APFS native clonefile and FSEvents APIs.


* *Windows*: Uses Projected File System (ProjFS) or falls back automatically.





### 1. Clone & Build

Clone the repository and build the workspace using Cargo:

```bash
git clone https://github.com/sys1own/aero-mitosis.git
cd aero-mitosis
cargo build --workspace
```

### 2. Run the Verification Suite

We include an extensive suite of unit and integration tests that simulate build failures, test the causal diagnostic engine, and verify the self-healing commit process:

```bash
cargo test --workspace
```

### 3. Run the Engine

Launch the primary workspace application[cite: 1]:

```bash
cargo run
```

```
