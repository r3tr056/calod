//! Phase 1 acceptance tests: retention, export gathering, and stats.
//! Deterministic via `ManualClock`, no HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use calod_core::store::config::FsyncStrategy;
use calod_ledger::*;
use calod_storage::wal::WalConfig;

const BASE_MS: i64 = 1_753_296_600_000;
const DAY_MS: i64 = 86_400_000;

fn scope() -> Scope {
    Scope {
        tenant_id: "tenant-1".to_string(),
        project_id: "project-1".to_string(),
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("calod-phase1-test-{tag}-{}", Uuid::new_v4()));
    p
}

fn config(dir: &std::path::Path) -> LedgerConfig {
    LedgerConfig::new(dir)
        .with_wal_config(WalConfig::new(dir.join("wal")).with_fsync_strategy(FsyncStrategy::Always))
}

async fn open(dir: &std::path::Path, clock: Arc<ManualClock>) -> Ledger {
    Ledger::open(config(dir), clock).await.expect("open ledger")
}

async fn committed_effect(ledger: &Ledger, scope: &Scope, run_id: Option<Uuid>, key: &str) -> Uuid {
    let decision = ledger
        .acquire(
            scope,
            key,
            AcquireEffectRequest {
                run_id,
                scope: "payments".to_string(),
                tool_name: "stripe.charge".to_string(),
                input: json!({"amount": 100}),
                lease_duration_seconds: Some(30),
                downstream_idempotency_supported: false,
            },
        )
        .await
        .expect("acquire");
    assert_eq!(decision.decision, EffectDecisionType::Execute);
    let token = decision.lease_token.clone().expect("lease token");
    let fence = decision.effect.fencing_token;
    ledger
        .transition(
            scope,
            decision.effect.id,
            UpdateEffectRequest {
                transition: EffectTransition::Start,
                lease_token: Some(token.clone()),
                fencing_token: fence,
                lease_duration_seconds: None,
                result: None,
                external_receipt: None,
                failure: None,
            },
            CallerCapabilities::default(),
        )
        .await
        .expect("start");
    ledger
        .transition(
            scope,
            decision.effect.id,
            UpdateEffectRequest {
                transition: EffectTransition::Commit,
                lease_token: Some(token),
                fencing_token: fence,
                lease_duration_seconds: None,
                result: Some(json!({"receipt": "r-1"})),
                external_receipt: None,
                failure: None,
            },
            CallerCapabilities::default(),
        )
        .await
        .expect("commit");
    decision.effect.id
}

async fn outcome_unknown_effect(ledger: &Ledger, scope: &Scope, key: &str) -> Uuid {
    let decision = ledger
        .acquire(
            scope,
            key,
            AcquireEffectRequest {
                run_id: None,
                scope: "payments".to_string(),
                tool_name: "stripe.charge".to_string(),
                input: json!({"amount": 200}),
                lease_duration_seconds: Some(30),
                downstream_idempotency_supported: false,
            },
        )
        .await
        .expect("acquire");
    let effect_id = decision.effect.id;
    ledger
        .transition(
            scope,
            effect_id,
            UpdateEffectRequest {
                transition: EffectTransition::Start,
                lease_token: decision.lease_token.clone(),
                fencing_token: decision.effect.fencing_token,
                lease_duration_seconds: None,
                result: None,
                external_receipt: None,
                failure: None,
            },
            CallerCapabilities::default(),
        )
        .await
        .expect("start");
    effect_id
}

#[tokio::test]
async fn retention_prunes_only_expired_terminal_data() {
    let dir = temp_dir("retention");
    let clock = Arc::new(ManualClock::new(BASE_MS));
    let ledger = open(&dir, clock.clone()).await;
    let scope = scope();

    // Old terminal run with a committed effect and events.
    let run = ledger
        .create_run(
            &scope,
            CreateRunRequest {
                external_id: "run-old".to_string(),
                agent_name: "agent".to_string(),
                metadata: None,
            },
        )
        .await
        .expect("create run");
    let old_effect = committed_effect(&ledger, &scope, Some(run.id), "key-old-0001").await;
    ledger
        .update_run(
            &scope,
            run.id,
            UpdateRunRequest {
                status: RunStatus::Completed,
            },
            None,
        )
        .await
        .expect("complete run");

    // outcome_unknown after lease expiry (must never be pruned).
    let unknown_effect = outcome_unknown_effect(&ledger, &scope, "key-unknown-1").await;
    clock.advance(31_000);
    let decision = ledger
        .acquire(
            &scope,
            "key-unknown-1",
            AcquireEffectRequest {
                run_id: None,
                scope: "payments".to_string(),
                tool_name: "stripe.charge".to_string(),
                input: json!({"amount": 200}),
                lease_duration_seconds: Some(30),
                downstream_idempotency_supported: false,
            },
        )
        .await
        .expect("reacquire expired");
    assert_eq!(decision.decision, EffectDecisionType::Reconcile);

    // Age everything by 10 days, then add a fresh committed effect.
    clock.advance(10 * DAY_MS);
    let fresh_effect = committed_effect(&ledger, &scope, None, "key-fresh-01").await;

    let report = ledger
        .apply_retention(RetentionPolicy {
            max_age_ms: 5 * DAY_MS,
        })
        .await
        .expect("apply retention");
    assert_eq!(report.runs_removed, 1, "old terminal run pruned");
    assert!(report.effects_removed >= 1, "old committed effect pruned");
    assert!(report.events_removed >= 2, "run events pruned with run");

    // Pruned data is gone; protected and fresh data remains.
    assert!(ledger.get_run(&scope, run.id).await.is_err());
    assert!(ledger.get_effect(&scope, old_effect).await.is_err());
    assert!(ledger.get_effect(&scope, unknown_effect).await.is_ok());
    assert!(ledger.get_effect(&scope, fresh_effect).await.is_ok());

    // Retention survives reopen (persisted as a WAL record).
    let clock2 = Arc::new(ManualClock::new(BASE_MS + 11 * DAY_MS));
    let reopened = open(&dir, clock2).await;
    assert!(reopened.get_run(&scope, run.id).await.is_err());
    assert!(reopened.get_effect(&scope, old_effect).await.is_err());
    assert!(reopened.get_effect(&scope, unknown_effect).await.is_ok());
    assert!(reopened.get_effect(&scope, fresh_effect).await.is_ok());
}

#[tokio::test]
async fn retention_then_snapshot_keeps_only_retained_data() {
    let dir = temp_dir("compact");
    let clock = Arc::new(ManualClock::new(BASE_MS));
    let ledger = open(&dir, clock.clone()).await;
    let scope = scope();
    let old = committed_effect(&ledger, &scope, None, "key-compact-old").await;
    clock.advance(10 * DAY_MS);
    let fresh = committed_effect(&ledger, &scope, None, "key-compact-new").await;
    let report = ledger
        .apply_retention(RetentionPolicy {
            max_age_ms: 5 * DAY_MS,
        })
        .await
        .expect("retention");
    assert_eq!(report.effects_removed, 1);
    ledger.snapshot().await.expect("snapshot compacts WAL");

    let reopened = open(&dir, Arc::new(ManualClock::new(BASE_MS + 10 * DAY_MS))).await;
    assert!(reopened.get_effect(&scope, old).await.is_err());
    assert!(reopened.get_effect(&scope, fresh).await.is_ok());
}

#[tokio::test]
async fn export_run_gathers_run_effects_events_and_stats_count() {
    let dir = temp_dir("export");
    let clock = Arc::new(ManualClock::new(BASE_MS));
    let ledger = open(&dir, clock).await;
    let scope = scope();
    let run = ledger
        .create_run(
            &scope,
            CreateRunRequest {
                external_id: "run-export".to_string(),
                agent_name: "agent".to_string(),
                metadata: None,
            },
        )
        .await
        .expect("run");
    let effect_id = committed_effect(&ledger, &scope, Some(run.id), "key-export-1").await;

    let export = ledger.export_run(&scope, run.id).await.expect("export");
    assert_eq!(export.run.id, run.id);
    assert_eq!(export.effects.len(), 1);
    assert_eq!(export.effects[0].id, effect_id);
    assert!(export.events.len() >= 2, "effect timeline events exported");
    assert!(verify_event_chain(&export.events), "export chain verifies");

    let stats = ledger.stats();
    assert!(stats.acquires_execute >= 1);
    assert!(stats.transitions_total >= 2);
    assert_eq!(stats.effects_current, 1);
    assert!(stats
        .effects_by_state
        .get("committed")
        .is_some_and(|n| *n == 1));
}
