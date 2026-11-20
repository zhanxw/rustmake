# rustmake

A fast, faithful Rust port of the [Snakemake](https://snakemake.github.io) workflow management system.

`rustmake` pairs a native Rust core engine (CLI/profiles, YAML/JSON parsing, JSON Schema validation, Snakefile lexer/transpiler, static linter, wildcard/pathvar resolution, DAG construction, persistence, scheduler, executors, benchmark monitor, and pure-Rust report/archive generation) with a persistent stdlib-only Python 3 co-process bridge (`src/runtime_shim.py`) for Snakefile evaluation, Python callables, `run:` blocks, checkpoints, and modules.

## Key Features

- **Snakemake-compatible CLI & Profiles**: Supports `--cores`, `--snakefile`, `--directory`, `--config` / `--configfile`, `--profile`, `--workflow-profile`, `--dry-run` (`-n`), `--printshellcmds` (`-p`), `--touch` (`-t`), `--forceall`, `--until`, `--omit-from`, `--batch`, `--unlock`, `--cleanup-metadata`, and `--delete-all-output` / `--delete-temp-output`.
- **DAG & Visualization**: Generates Graphviz DOT (`--dag`, `--rulegraph`, `--filegraph`), Mermaid (`--print-dag-as mermaid-js`), and D3 JSON (`--d3dag`).
- **Linting & Summaries**: Built-in `--lint [plain|json]`, `--list-rules` (`-l`), `--lt`, `--summary`, `--detailed-summary` (`-D`), and `--list-untracked` / input / params / code changes.
- **Execution & Persistence**: Resource-aware parallel scheduler (`rayon` / scoped workers), `shadow:` modes (`full`, `shallow`, `minimal`, `copy-minimal`), cluster submission (`--cluster`), output caching, benchmarking, and file or bundled SQLite (`--persistence-backend db`) metadata backends.
- **Reports & Archiving**: Pure-Rust standalone HTML (`--report report.html`), ZIP (`--report report.zip`), and directory (`--report dir/`) reports with optional custom CSS stylesheets, plus `.tar` / `.tar.gz` archiving (`--archive`).

## Prerequisites & Installation

- **Runtime requirement**: `python3` available on `PATH` (standard library only; no `pip` dependencies required).

Install from crates.io:

```bash
cargo install rustmake
```

Or build from source:

```bash
cargo build --release
./target/release/rustmake --version
```

## Usage

```bash
# Dry-run workflow and print shell commands
rustmake -n -p

# Execute with up to 4 cores
rustmake --cores 4

# Lint a Snakefile (plain text or JSON)
rustmake --lint
rustmake --lint json

# Generate DAG in DOT or Mermaid format
rustmake --dag
rustmake --dag --print-dag-as mermaid-js

# Generate a self-contained HTML or ZIP workflow report
rustmake --report report.html
rustmake --report report.zip
```

## Running Tests

```bash
# Run Rust unit, lint, example, and workflow integration test suites
cargo test

# Run the Python unittest CLI/workflow test harness
python3 tests/run_rustmake_tests.py
```

## License & Attribution

Licensed under the [MIT License](LICENSE.md). Inspired by and compatible with [Snakemake](https://github.com/snakemake/snakemake) (Copyright (c) 2012-2024 The Snakemake team).
