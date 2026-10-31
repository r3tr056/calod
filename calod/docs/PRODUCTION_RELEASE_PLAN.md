# Calod — Stable v1 Development Plan

Status: **Final** (accepted by founder, 2026-10-14)

This plan's focus is development first: complete a stable v1 of
Calod. How and when to release is deliberately not in this plan;
release mechanics are parked in Section 8 and each returns for its
own decision once stable v1 is declared. There are no calendar
dates: work is phase-gated, and a phase starts only when the
previous phase's exit gate passes.

Decision record: `PRODUCT_STRATEGY_V2_REFINED.md` remains the
strategy (market, positioning, ICP, offer ladder). This plan
supersedes its roadmap/sequencing where they conflict. The
archived `IMPLEMENTATION_PLAN.md` ("faster Redis") is rejected.

## Goal

Calod is the durable action ledger for AI agents: when an agent
retries a tool call, Calod prevents the retry from repeating a
real-world side effect, records a tamper-evident receipt, and
makes uncertain outcomes recoverable. Stable v1 means that claim
is completely implemented, hardened, and proven by tests on a
single node — not asserted in a README.

Grounding facts (verified in-repo at `d977baf` on `dev`/`main`):
the Cargo workspace (`calod-core`, `calod-resp`, `calod-storage`,
`calod-ledger`, `calod-api`, `calod` binary), the effect state
machine with leases, fencing tokens, input-digest 409 conflicts,
`outcome_unknown` + reconcile, runs/events with SHA-256 hash
chains, binary-safe CRC32C WAL with fail-closed recovery, all 16
REST operations under `/v1`, TLS + scoped API keys, unified
config, the Python SDK, and a frozen RESP store already exist.
Gates at that commit: `cargo fmt --check` clean, `cargo clippy
--all-targets -- -D warnings` clean, `cargo test --all-targets`
293 passed / 0 failed.

## Key decisions (settled)

1. Fully open source: server and SDKs MIT (existing `LICENSE`
   retained). License-key activation, BUSL, and billing-as-feature
   are out of scope permanently.
2. Core-first scope: retention/compaction, evidence export,
   Prometheus metrics, SDK polish + adapter examples, docs and
   hardening are in. TypeScript SDK, console UI, hosted cloud,
   and framework integrations beyond examples are post-v1.
3. RESP store frozen as a secondary compatibility/inspection
   interface: no new commands, no production claim, bug/security/
   crash fixes only, existing tests as the freeze guard.
4. Honest single-node production bar: no HA claim. Node loss is
   downtime; backups are the data-safety story, stated plainly.
5. No dates, no release planning in this plan: phase gates and
   passing criteria decide progress. Revenue is not a v1
   criterion; making it work comes first.

## Success criteria — "stable v1" means all of these

- All Phase 0–4 exit gates below pass, evidenced in the repo.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets --
  -D warnings`, and `cargo test --all-targets` are green, with
  the test count grown from the 293 baseline, never shrunk.
- Zero acknowledged writes are lost across randomized `kill -9`
  crash testing; WAL/snapshot corruption fails closed, always.
- A tampered evidence export fails verification; a genuine one
  verifies end to end with an independent script.
- In every adapter example, a deliberate double-invoke produces
  exactly one provider call; the retry replays the receipt.
- A fresh user, following only the quickstart on a clean machine,
  protects a first real tool call in under 10 minutes — twice,
  on two different OSes.
- The README's claims match reality: every stated capability is
  implemented and tested, every limitation (single node, no HA,
  frozen RESP) is stated.

## Approach

Build only what is missing, in dependency order, on `dev`; `main`
stays untouched until stable v1 is declared. Each phase names
its work surfaces, its verification (automated tests proving we
built it right), its validation (evidence it solves the user's
problem), binary passing criteria, and its completion milestone.
A failed criteria keeps the phase open; scope never expands
inside a phase — new ideas go to Section 8.

## Steps

### Phase 0 — Fix what is broken, make the repo buildable by strangers

- Work: fix `calod/Dockerfile` for the workspace layout (copy
  `crates/`, build and ship the `calod` binary); fix CI (drop the
  deleted `v1-implementation` branch, add Docker-build and
  Python-SDK test jobs); write `CONTRIBUTING.md`; correct the
  README build/run instructions for the workspace and ledger API;
  add `CHANGELOG.md` with the semver policy it will follow.
- Verification: every CI job green including Docker build; SDK
  `pytest` suite exists and passes (client/decorator happy paths).
- Validation: a clean clone, following only the README, reaches a
  running server and a successful `POST /v1/effects`.
- Passing criteria: CI all-green; README walkthrough verified
  from a clean checkout.
- Milestone: **M0 — repo is buildable and runnable by strangers.**

### Phase 1 — Complete the evidence core: retention, export, metrics

- Work: retention policies + compaction for effects/events/WAL
  in `calod-ledger`/`calod-storage`, configured through
  `calod/src/config.rs`; evidence export in `calod-api`
  (JSON + CSV + signed manifest over runs/effects/events, with
  matching additions to `calod/docs/openapi.yaml`); Prometheus
  metrics for the REST API and ledger (acquire/commit/reconcile,
  lease waits, WAL health) alongside health endpoints.
- Verification: tests prove retention deletes only expired data,
  exports verify against the hash chain, the manifest signature
  verifies, the metrics scrape parses, and tampering with any
  exported record breaks verification.
- Validation: an exported evidence bundle for a sample run opens
  in a spreadsheet and verifies end to end with a script that
  shares no code with the exporter.
- Passing criteria: all new tests green; tampered export fails;
  full workspace suite stays green.
- Milestone: **M1 — the evidence story is complete and provable.**

### Phase 2 — Python SDK production polish and adapter examples

- Work: harden `calod/sdk/python` (typed models checked against
  `openapi.yaml`, explicit retry/timeout semantics, sync/async
  parity); build the real test suite under `sdk/python/tests`
  (mock-transport unit tests plus live-server integration tests);
  runnable examples under `sdk/python/examples/` for Stripe,
  Razorpay/UPI (including the `outcome_unknown` "pending" path),
  WhatsApp Business, and email.
- Verification: `pytest` green in CI for unit tests; integration
  tests green against a locally built server.
- Validation: each example runs end to end against a stubbed
  provider and demonstrates the core claim — a duplicate retry
  replays the receipt and never re-calls the provider.
- Passing criteria: all examples exit 0; a deliberate
  double-invoke in every example yields exactly one provider call.
- Milestone: **M2 — a Python user can protect a real tool call.**

### Phase 3 — Hardening: prove the single-node production claim

- Work: randomized `kill -9` crash-recovery harness (10k state
  transitions, acknowledged-state comparison); automated
  backup/restore verification in CI; fuzz or property tests for
  the RESP parser and REST extractors; `cargo audit` in CI;
  24-hour mixed soak (REST + RESP + restarts); reproducible
  ledger-API throughput numbers from `calod/scripts/benchmark/`.
- Verification: harnesses run as CI jobs or scripted recorded
  runs; zero acknowledged-write loss; every corruption case
  fails closed; audit reports no unaddressed advisories.
- Validation: the soak report shows no corruption, no unbounded
  growth, and no acknowledged-state divergence over 24 hours;
  benchmark numbers reproduce from the harness README.
- Passing criteria: every Phase-3 gate is evidenced by a report
  committed under `calod/docs/validation/` or linked CI artifacts.
- Milestone: **M3 — production claim is proven, not asserted.**

### Phase 4 — Documentation and fresh-user validation

- Work: quickstart (<10 minutes to first protected effect);
  deployment runbook (install, configuration, TLS, API keys,
  backup/restore, upgrade/rollback); regulation-mappings page
  (EU AI Act Art. 12/26, SOC 2 CC, ISO 42001 A.9/A.10, DPDP Act,
  RBI expectations — tooling mappings, explicitly not legal
  advice); RESP supported/unsupported command matrix; honesty
  pass on README claims and limitations.
- Verification: doc links checked in CI; every command in the
  quickstart and runbook is executed verbatim by a scripted test.
- Validation: a fresh user who is not the author, following only
  the quickstart, protects a first effect in under 10 minutes;
  the timing is recorded.
- Passing criteria: fresh-user test passes twice, on two
  different operating systems.
- Milestone: **M4 — stable v1 declared.** Work stops here for a
  founder review; nothing in Section 8 starts automatically.

## Validation plan (applies to every phase)

Local gates, run from `calod/` before any phase is declared
complete: `cargo fmt --all -- --check`; `cargo clippy
--all-targets -- -D warnings`; `cargo test --all-targets`;
`pytest` for `calod/sdk/python`. CI must show the same gates
green plus Docker build. Each phase additionally carries its
own verification/validation pair above; the highest-risk
validation in the whole plan is Phase 3's crash-recovery
harness, because the product's entire promise reduces to
"an acknowledged effect survives a crash exactly once."

## Risks

- Single node is a single point of failure; the runbook must make
  backup/restore a habit, and the docs must never imply HA.
- Building privately with no external users until v1 raises
  market-fit risk; the mitigations inside this plan are the
  adapter examples (real provider shapes) and Phase 4's
  fresh-user tests. Full market validation is deferred with
  the release, knowingly.
- The frozen RESP surface will attract Redis-expectation issues;
  the support matrix is the deflection mechanism, and new RESP
  commands are rejected by decision 3, not by mood.
- Retention/export touch the WAL and hash chain — the two places
  a silent bug destroys the evidence claim. They get the
  tamper-fails tests first, not last.

## Section 8 — Parked (not in this plan, not approved by it)

Release mechanics (artifacts, channels, versioning ceremony
beyond `CHANGELOG.md`, launch, publicity), public visibility
timing, TypeScript SDK, console UI, hosted/beta cloud, framework
integrations beyond examples, clustering/replication/failover,
and adoption/revenue programmes. Each item returns for its own
decision when stable v1 (M4) is declared. Accepting this plan
approves Phases 0–4 only.

Open questions: none. Execution details discovered inside a
phase (exact retention defaults, metric names, example provider
stubs) are decided in that phase without reopening scope.
