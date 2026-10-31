# Contributing to Calod

Calod is the durable action ledger for AI agents. The implementation
plan is `calod/docs/PRODUCTION_RELEASE_PLAN.md` (Phases 0–4, phase
gated, no dates); the product strategy is
`calod/docs/PRODUCT_STRATEGY_V2_REFINED.md`. Work lands on `dev`;
`main` is the stable branch and only advances at a release.

## Prerequisites

- Rust (stable, via [rustup](https://rustup.rs)) with `rustfmt` and
  `clippy` components.
- Python 3.10+ for the SDK (optional, only for SDK work).
- Docker (optional, only for the container build).

## Build and run

```bash
cd calod
cargo build            # debug build of the workspace
cargo run              # RESP on 127.0.0.1:6379, ledger API on 127.0.0.1:7379
```

Configuration is one source of truth — CLI flags >
`CALOD_*` environment variables > `calod.toml` > defaults; see
`calod/src/config.rs` for the full variable list.

## Checks (all must pass before a change is done)

From `calod/`:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

For the Python SDK, from `calod/sdk/python/`:

```bash
pip install -e ".[dev]"
pytest -q
```

CI (`.github/workflows/ci.yml`) runs the same gates plus a Docker
build and OpenAPI lint. Do not weaken, skip, or delete a failing
test to get green; fix the code, or explain why the test's
expectation is wrong in the change description.

## Conventions

- Smallest change that fully solves the problem; match surrounding
  naming, error handling, and idiom. No new dependencies unless the
  task needs them.
- Every behaviour change lands with tests in the project's normal
  test layout (Rust integration tests in `calod/tests/`, crate unit
  tests beside the code, SDK tests in `calod/sdk/python/tests/`).
- The RESP surface is frozen (stable v1 plan, decision 3): no new
  Redis commands; bug, security, and crash fixes only.
- Docs live with the code they describe. Public claims must be
  implemented and tested; limitations (single node, no HA) are
  stated, never softened.
- Record user-visible changes in `CHANGELOG.md` under Unreleased.

## Branches and commits

- Branch from `dev`, open pull requests against `dev`.
- Do not push directly to `main`.
- Write commit messages that say what changed and why; keep each
  commit scoped to one plan phase or fix.
