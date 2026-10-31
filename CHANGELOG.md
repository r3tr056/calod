# Changelog

All notable changes to Calod are recorded here.

## Versioning policy

Calod follows [Semantic Versioning](https://semver.org) from
v1.0.0 onward:

- **MAJOR** — incompatible changes to the REST API (`/v1`), the
  Python SDK's public API, the WAL/snapshot on-disk formats
  without a migration path, or the effect state machine.
- **MINOR** — backwards-compatible additions (new endpoints, new
  optional configuration, new SDK helpers).
- **PATCH** — backwards-compatible fixes, including durability
  and crash-recovery fixes.

The RESP interface is a frozen compatibility surface: it follows
the same version as the server and gains no new commands. Pre-1.0
versions (0.x) may break any interface without a major bump.

## Unreleased

### Added

- Phase 2 — Python SDK hardening: explicit timeout/retry
  semantics on both clients (idempotent calls only), new
  `export_run` / `apply_retention` / `metrics` methods with typed
  `RunExport` / `RetentionReport` models, sync/async parity test,
  OpenAPI drift-guard tests, live-server integration tests, and
  runnable adapter examples (Stripe, Razorpay/UPI with the
  `outcome_unknown` reconcile path, WhatsApp, email) under
  `calod/sdk/python/examples/`.
- Phase 1 — evidence export: `GET /v1/runs/{run_id}/export`
  (JSON bundle with HMAC-signed manifest, or event CSV) and the
  independent verifier `calod/scripts/verify_export.py`.
- Phase 1 — retention: `POST /v1/admin/retention` (admin scope)
  and `CALOD_API__RETENTION_MAX_AGE_DAYS` /
  `CALOD_API__RETENTION_APPLY_ON_STARTUP` configuration; pruning
  is WAL-persisted and snapshot compaction truncates the WAL.
- Phase 1 — Prometheus metrics at `GET /metrics` (acquires by
  decision, transitions, retention, per-state effect gauges).
- Server `--create-api-key` one-shot provisioning for the first
  REST API key.
- Production release plan (`calod/docs/PRODUCTION_RELEASE_PLAN.md`):
  stable-v1, phase-gated development plan (Phases 0–4).
- Python SDK test suite (`calod/sdk/python/tests/`).
- CI jobs for the Docker build and the Python SDK tests.
- `CONTRIBUTING.md` and this changelog.

### Fixed

- Dockerfile now builds the Cargo workspace (copies `crates/`,
  builds the `calod` package, exposes the ledger API port 7379).
- README build/run instructions corrected for the workspace
  layout and the ledger REST API (they predated both).
- CI branch list no longer references the deleted
  `v1-implementation` branch.
