//! Phase 1 exit tests over HTTP: signed evidence export, Prometheus
//! metrics, and admin retention against the real `calod` binary.
//!
//! The JSON export is verified by `scripts/verify_export.py`, which
//! shares no code with the exporter: it recomputes the event hash
//! chain, the bundle digest, and the manifest HMAC signature.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tokio::process::{Child, Command as ProcCommand};

fn temp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("calod-phase1-http-{}", uuid::Uuid::new_v4()));
    p
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn create_api_key(data_dir: &Path, scopes: &[&str]) -> String {
    let store = calod::store::api_keys::ApiKeyStore::open(data_dir).expect("open key store");
    let (_meta, secret) = store
        .create(
            "tenant-test",
            "project-test",
            scopes.iter().map(|s| s.to_string()).collect(),
            None,
        )
        .expect("create api key");
    secret
}

struct ServerProc {
    // Held so `kill_on_drop` stops the server when the test ends.
    #[allow(dead_code)]
    child: Child,
    api_port: u16,
}

impl ServerProc {
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.api_port)
    }
    fn root(&self) -> String {
        format!("http://127.0.0.1:{}", self.api_port)
    }
}

async fn start_server(data_dir: &Path) -> ServerProc {
    let resp_port = free_port();
    let api_port = free_port();
    std::fs::create_dir_all(data_dir).expect("temp dir");
    let child = ProcCommand::new(env!("CARGO_BIN_EXE_calod"))
        .args(["--address", "127.0.0.1", "--port"])
        .arg(resp_port.to_string())
        .current_dir(data_dir)
        .env("CALOD_PERSISTENCE__ENABLE_SNAPSHOT", "false")
        .env("CALOD_PERSISTENCE__SNAPSHOT_DIR", data_dir)
        .env("CALOD_PERSISTENCE__ENABLE_AOF", "false")
        .env("CALOD_PERSISTENCE__SNAPSHOT_INTERVAL_SECS", "3600")
        .env("CALOD_API__ENABLED", "true")
        .env("CALOD_API__LISTEN_ADDR", format!("127.0.0.1:{api_port}"))
        .env("CALOD_API__DATA_DIR", data_dir.join("ledger"))
        .env("CALOD_API__DURABILITY", "always")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn calod server");
    let server = ServerProc { child, api_port };
    let client = Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(resp) = client
            .get(format!("{}/health/ready", server.base()))
            .send()
            .await
        {
            if resp.status() == StatusCode::OK {
                break;
            }
        }
        assert!(std::time::Instant::now() < deadline, "server not ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    server
}

async fn run_verify_script(
    export_path: &Path,
    key_path: Option<&Path>,
    csv: bool,
) -> (bool, String) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/verify_export.py");
    for python in ["python", "python3"] {
        let mut cmd = tokio::process::Command::new(python);
        cmd.arg(&script).arg(export_path);
        if let Some(key) = key_path {
            cmd.arg("--key-file").arg(key);
        }
        if csv {
            cmd.arg("--csv");
        }
        match cmd.output().await {
            Ok(out) => {
                return (
                    out.status.success(),
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    ),
                );
            }
            Err(_) => continue,
        }
    }
    panic!("no python interpreter found for verify_export.py");
}

/// Create a run and one committed effect on it; returns (run_id, effect_id).
async fn run_with_committed_effect(
    client: &Client,
    server: &ServerProc,
    key: &str,
) -> (String, String) {
    let resp = client
        .post(format!("{}/runs", server.base()))
        .header("x-calod-key", key)
        .header("idempotency-key", "run-create-1")
        .json(&json!({ "external_id": "run-phase1", "agent_name": "order-agent" }))
        .send()
        .await
        .expect("create run");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run: Value = resp.json().await.unwrap();
    let run_id = run["id"].as_str().unwrap().to_string();

    let resp = client
        .post(format!("{}/effects", server.base()))
        .header("x-calod-key", key)
        .header("idempotency-key", "effect-phase1-1")
        .json(&json!({
            "scope": "order-1",
            "tool_name": "stripe.charge",
            "input": { "amount_minor": 5000 },
            "run_id": run_id,
        }))
        .send()
        .await
        .expect("acquire");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let decision: Value = resp.json().await.unwrap();
    let effect_id = decision["effect"]["id"].as_str().unwrap().to_string();
    let lease = decision["lease_token"].as_str().unwrap().to_string();
    let fence = decision["effect"]["fencing_token"].as_i64().unwrap();

    for transition in [
        json!({ "transition": "start", "lease_token": lease, "fencing_token": fence }),
        json!({ "transition": "commit", "lease_token": lease, "fencing_token": fence, "result": { "charged": 5000 } }),
    ] {
        let resp = client
            .patch(format!("{}/effects/{}", server.base(), effect_id))
            .header("x-calod-key", key)
            .header("if-match", format!("\"{fence}\""))
            .json(&transition)
            .send()
            .await
            .expect("transition");
        assert_eq!(resp.status(), StatusCode::OK, "transition must succeed");
    }
    (run_id, effect_id)
}

#[tokio::test]
async fn export_json_verifies_with_independent_script_and_csv_parses() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let key = create_api_key(
        &dir,
        &[
            "effects:read",
            "effects:write",
            "effects:reconcile",
            "admin",
        ],
    );
    let server = start_server(&dir).await;
    let client = Client::new();
    let (run_id, _effect) = run_with_committed_effect(&client, &server, &key).await;

    // JSON export.
    let resp = client
        .get(format!("{}/runs/{}/export", server.base(), run_id))
        .header("x-calod-key", &key)
        .send()
        .await
        .expect("export json");
    assert_eq!(resp.status(), StatusCode::OK);
    let bundle: Value = resp.json().await.unwrap();
    assert!(bundle["manifest"]["signature"].is_string());
    assert!(bundle["events"].as_array().is_some_and(|e| e.len() >= 2));

    let export_path = dir.join("export.json");
    std::fs::write(&export_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
    let key_file = dir.join("ledger").join("export_signing.key");
    assert!(key_file.exists(), "server persists its export signing key");
    let (ok, out) = run_verify_script(&export_path, Some(&key_file), false).await;
    assert!(ok, "independent verifier must accept the export: {out}");
    assert!(out.contains("EXPORT VERIFIED"), "{out}");

    // Tampered export must fail (change a committed receipt).
    let mut tampered = bundle.clone();
    tampered["effects"][0]["result"]["charged"] = json!(1);
    tampered["events"][0]["payload"]["detail"] = json!("forged");
    let tampered_path = dir.join("export-tampered.json");
    std::fs::write(&tampered_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
    let (ok, out) = run_verify_script(&tampered_path, Some(&key_file), false).await;
    assert!(!ok, "tampered export must fail verification: {out}");

    // CSV export parses as a spreadsheet-shaped table and verifies.
    let resp = client
        .get(format!(
            "{}/runs/{}/export?format=csv",
            server.base(),
            run_id
        ))
        .header("x-calod-key", &key)
        .send()
        .await
        .expect("export csv");
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp
        .headers()
        .get("x-calod-export-signature")
        .is_some_and(|v| !v.is_empty()));
    let csv_text = resp.text().await.unwrap();
    let mut lines = csv_text.lines();
    assert_eq!(
        lines.next(),
        Some("run_id,sequence,type,occurred_at,recorded_at,previous_hash,hash,payload")
    );
    assert!(lines.count() >= 2, "CSV carries the event rows");
    let csv_path = dir.join("events.csv");
    std::fs::write(&csv_path, &csv_text).unwrap();
    let (ok, out) = run_verify_script(&csv_path, None, true).await;
    assert!(ok, "CSV linkage must verify: {out}");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn metrics_scrape_exposes_ledger_counters() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let key = create_api_key(&dir, &["effects:read", "effects:write", "admin"]);
    let server = start_server(&dir).await;
    let client = Client::new();
    let _ = run_with_committed_effect(&client, &server, &key).await;

    let resp = client
        .get(format!("{}/metrics", server.root()))
        .send()
        .await
        .expect("metrics");
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    for needle in [
        "calod_api_up 1",
        "calod_acquires_total{decision=\"execute\"}",
        "calod_transitions_total",
        "calod_effects_current{state=\"committed\"} 1",
        "# TYPE calod_effects_current gauge",
    ] {
        assert!(
            text.contains(needle),
            "metrics must contain '{needle}':\n{text}"
        );
    }
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn retention_requires_admin_and_prunes_committed_effects() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let plain_key = create_api_key(&dir, &["effects:read", "effects:write"]);
    let admin_key = create_api_key(&dir, &["effects:read", "effects:write", "admin"]);
    let server = start_server(&dir).await;
    let client = Client::new();
    let (_run_id, effect_id) = run_with_committed_effect(&client, &server, &admin_key).await;

    // Without the admin scope: 403.
    let resp = client
        .post(format!("{}/admin/retention", server.base()))
        .header("x-calod-key", &plain_key)
        .json(&json!({ "max_age_days": 0 }))
        .send()
        .await
        .expect("retention forbidden");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // With admin: the old committed effect is pruned.
    let resp = client
        .post(format!("{}/admin/retention", server.base()))
        .header("x-calod-key", &admin_key)
        .json(&json!({ "max_age_days": 0, "compact": true }))
        .send()
        .await
        .expect("retention");
    assert_eq!(resp.status(), StatusCode::OK);
    let report: Value = resp.json().await.unwrap();
    assert!(report["effects_removed"].as_u64().unwrap() >= 1, "{report}");

    let resp = client
        .get(format!("{}/effects/{}", server.base(), effect_id))
        .header("x-calod-key", &admin_key)
        .send()
        .await
        .expect("get pruned effect");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
