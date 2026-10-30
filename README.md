# gleam-project-model

Tier-0 project-model, build-system, and test-runner plugin for Gleam packages
in Basalt.

## Capabilities

- `PROJECT_MODEL` / `project-model:gleam`: emits the generic project-model
  schema for a `gleam.toml` package (source, test, and build roots plus path
  owners).
- `BUILD_SYSTEM`: exposes `gleam build`.
- `TEST_RUNNER`: exposes `gleam test`.
- `REVIEW_ACTIONS`: build, test, and `gleam format --check src test`, with
  line parsing for compiler diagnostics and gleeunit summaries.

## Project model

- Package name/version/target and dependency lists are parsed from
  `gleam.toml`.
- A package is marked `app` / `gleam-application` when
  `src/<package>.gleam` declares `pub fn main(`; otherwise it is a
  `library` / `gleam-package`.
- Owners: `gleam.toml` and `manifest.toml` (exact), `src/`, `test/`,
  `build/` (prefix).
- `build/` is the Gleam compiler output directory and should be ignored by
  the workspace loader.

## Review output parsing

- `error:` / `warning:` headers become `build` / `diagnostic` entries.
- `┌─ path:line:col` locations attach the offending file to an entry.
- `N passed, M failures` / `N passed, no failures` closes the test run with
  exit code 0 or 1.
- ANSI colour sequences are stripped before matching.

## Build

```bash
cargo build --release --target wasm32-unknown-unknown
```

Artifact: `target/wasm32-unknown-unknown/release/gleam_project_model.wasm`.
