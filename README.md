# Aero-Mitosis 🧬

**Autonomic CI Engine & Self-Healing Build Orchestrator**

[![Crates.io](https://img.shields.io/crates/v/aero-mitosis.svg)](https://crates.io/crates/aero-mitosis)
[![Docs.rs](https://docs.rs/aero-mitosis/badge.svg)](https://docs.rs/aero-mitosis)
[![CI](https://github.com/sys1own/aero-mitosis/actions/workflows/ci.yml/badge.svg)](https://github.com/sys1own/aero-mitosis/actions)

Welcome to **Aero-Mitosis**, a high-performance build orchestration engine designed to treat your software compilation process as a dynamic, self-healing system.

Aero-Mitosis is a command‑line tool that sits in your workspace, monitors your builds in real‑time, diagnoses the root cause for any failure, and repairs your configuration so you can get right back to coding.

---

## Why Aero-Mitosis?

* **Self-Healing Loops**: When a build fails due to configuration drift or environmental mismatches, our `SelfHealingRouter` creates an isolated sandbox, applies potential fixes (e.g., restoring a corrupted file from Git), and commits the solution only after verifying it passes all tests.

* **Causal Inference**: Instead of guessing why a build failed, the engine calculates a **causal gradient** to mathematically pinpoint the exact dependency or flag modification that caused the failure.

* **Zero-Cost Virtualization**: We use native OS primitives (like `clonefile` on macOS, ProjFS on Windows, and a lightweight CoW fallback on Linux) to create "copy‑on‑write" sandbox overlays. You get complete build isolation without the massive overhead of full containers.

* **Wavefront Scheduling**: Our work‑stealing scheduler breaks your build into parallel "wavefronts," maximising CPU utilisation while respecting dependency boundaries.

---

## System Architecture

The workspace is split into three core crates alongside a primary execution binary, giving you modular power with zero monolithic fluff:

```text
aero-mitosis/
├── Cargo.toml
├── src/
│   └── main.rs                     # Primary binary launcher
└── crates/
    ├── autonomic_ci_core/          # Core scheduling, diagnostics & self-healing
    ├── autonomic_ci_parser/        # AST analysis & SCM ingestion
    └── autonomic_ci_virtualizer/   # Cross-platform filesystem sandboxing
```

### 1. `autonomic_ci_core`

The brain of the operation. It houses the intelligence required to schedule builds, calculate diagnostic gradients, and execute self-healing loops:

* **Scheduler Module (`scheduler/`)**: Features **wavefront scheduling** (`wavefront.rs`) and **work‑stealing tasks** (`work_stealing.rs`) to saturate CPU cores while maintaining strict dependency graph order.

* **Diagnostics Engine (`diagnostics/`)**: Includes **causal gradient analysis** (`causal_gradient.rs`) for failure tracking and an **FFI normalizer** (`ffi_normalizer.rs`) for safe cross‑language boundaries.

* **Self‑Healing Router (`self_healing/`)**: Houses the autonomous router (`router.rs`) to isolate, fix, and re‑verify broken build state candidates.

* **Verification Suite (`tests/end_to_end.rs`)**: An extensive end‑to‑end integration test suite simulating real build failures, testing causal diagnostics, and confirming self‑healing commits.

### 2. `autonomic_ci_parser`

Responsible for analysing and digesting project structures and source control information:

* **AST Analysis (`ast/`)**: Leverages **Tree‑sitter** to ingest source code and manifests, utilising matrix mapping (`matrix_mapper.rs`) and AST query engines (`queries.rs`) to form a structural dependency graph.

* **SCM Ingestion (`scm/`)**: Ingests repository state (`ingestion.rs`) to evaluate incoming commits, branches, and patch matrices.

### 3. `autonomic_ci_virtualizer`

The OS‑agnostic virtualization layer that manages lightweight sandboxed file isolation:

* **macOS Engine**: High‑speed APFS snapshotting and cloning (`macos_apfs.rs`) paired with `FSEvents` filesystem monitoring (`macos_fsevents.rs`).

* **Windows Engine**: Native Projected File System (`windows_projfs.rs`) integration alongside automated filesystem fallback routines (`windows_fallback.rs`).

* **Linux Engine**: A performant copy‑on‑write fallback (`cow_engine.rs`) that uses hard‑links and copy‑up semantics, ensuring zero‑cost isolation on all major distributions.

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

* **Rust Toolchain**: Install the latest stable Rust from [rustup.rs](https://rustup.rs/).
* **Supported Platforms**:
  * Linux (Ubuntu 20.04+, RHEL 8+, etc.)
  * macOS 11+
  * Windows 10/11 (ProjFS recommended, fallback available)
* **Build Tools**: For C/C++ projects, ensure `gcc`, `make`, `cmake`, and `m4` are installed. Python projects need `python3` and `pip3`.

### Installation

Install Aero‑Mitosis directly from [crates.io](https://crates.io/crates/aero-mitosis):

```bash
cargo install aero-mitosis
```

Alternatively, build from source:

```bash
git clone https://github.com/sys1own/aero-mitosis.git
cd aero-mitosis
cargo build --release --workspace
# The binary is now at target/release/aero-mitosis
```

### Basic Usage

Navigate to the repository you want to build and run:

```bash
aero-mitosis /path/to/your/repo
```

The tool will:
1. **Ingest** your repository’s structure and dependency graph.
2. **Schedule** build units into parallel wavefronts.
3. **Execute** the build inside a lightweight, isolated sandbox.
4. **Monitor** for failures and – if any occur – attempt to self‑heal by restoring corrupted files or applying configuration patches.
5. **Commit** the successful result back to the sandbox (your original source remains untouched).

**Exit Codes**:
- `0` – Build succeeded (or was successfully repaired).
- `1` – Unrecoverable user error (e.g., directory does not exist).
- `2` – Build failed and could not be healed.

### Advanced Usage

**Enable Debug Logging**

To see exactly what the engine is doing under the hood, set the `RUST_LOG` environment variable:

```bash
RUST_LOG=debug aero-mitosis /path/to/your/repo
```

**Set a Timeout**

Use the `--timeout` flag to limit the build time per unit (default: 900 seconds):

```bash
aero-mitosis /path/to/your/repo --timeout 600
```

**Dry‑Run Mode (Analysis Only)**

If you want to inspect the dependency graph without actually building, you can run the tool with `--dry-run` (if implemented) or simply check the logs with `RUST_LOG=info`.

---

## A Real‑World Self‑Healing Example

Suppose you accidentally corrupt a critical Python file in your repository:

```bash
echo "syntax error" >> src/accelerator/config.py
```

When you run Aero‑Mitosis, the build fails. The engine:

1. **Detects** the failure during `pip install -e .`.
2. **Analyses** the causal gradient to pinpoint `config.py` as the root cause.
3. **Generates** a patch by restoring the file from the Git history (`git show HEAD:./src/accelerator/config.py`).
4. **Applies** the patch and re‑runs the build.
5. **Commits** the repair – your build now succeeds!

All this happens automatically, without any user intervention.

---

## Testing

We take correctness seriously. The project includes a comprehensive test suite that runs on every commit:

```bash
cargo test --workspace
cargo test --doc
cargo clippy -- -D warnings
cargo fmt --check
```

These tests cover:
- Unit tests for wavefront scheduling, causal gradient, and work‑stealing.
- Integration tests that simulate full self‑healing cycles with a mock validator.
- Real‑world end‑to‑end scenarios against actual repositories (e.g., `aero-accelerator`), verifying clean builds, corrupted‑file restoration, permission handling, and incremental caching.

CI runs on **Ubuntu**, **macOS**, and **Windows** to ensure cross‑platform reliability.

---

## License

- MIT License ([LICENSE-MIT](LICENSE-MIT))

---

## Acknowledgements

Aero‑Mitosis stands on the shoulders of giants. Special thanks to the Rust community, Tree‑sitter, and the open‑source projects that make autonomic computing a reality.

---
