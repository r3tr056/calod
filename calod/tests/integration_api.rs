//! Phase P2: REST API contract tests.
//!
//! These tests drive the real `calod` binary as a child process (same pattern
//! as `integration_backup_restore.rs`): the RESP listener gets one ephemeral
//! port, the REST API another (`CALOD_API__LISTEN_ADDR`), persistence and the
//! API-key store are rooted at a per-test temp dir, and the ledger runs with
//! `CALOD_API__DURABILITY=always` so crash-durability assertions are exact.
//!
//! Covered:
//!
//! - health/readiness/version response shapes, `X-Request-ID` on responses
//! - auth: missing/invalid key → 401 problem+json; valid key works; reconcile
//!   without the `effects:reconcile` scope → 403
//! - the P2 exit suite over HTTP:
//!   - (a) `p2_concurrent_acquire_one_execute_then_replays` — 100 concurrent
//!     acquires on one idempotency key yield exactly one 201 execute (the
//!     harness-executed tool ran once), the rest 202 wait; after commit,
//!     100 retries all replay the identical receipt
//!   - (b) `p2_stale_fencing_token_conflict` — stale fencing token PATCH → 409
//!   - (c) `p2_same_key_different_input_conflict` — same key, other input → 409
//!   - (d) `p2_crash_before_commit_recovers_as_reconcile` — kill after
//!     acquire+start, restart, lease expiry → 200 reconcile, never execute
//!     again until reconciled
//!   - (e) `run_etag_if_match_happy_path_and_412` — ETag/If-Match round-trip
//!     plus a 412 precondition failure
//! - cursor pagination envelope per the OpenAPI contract

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};
use tokio::process::{Child, Command as ProcCommand};

fn temp_dir() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("calod-api-test-{}", uuid::Uuid::new_v4()));
    p
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Create an API key in the server's key store (`api_keys.bin` lives in the
/// persistence snapshot dir) before the server starts. Returns the one-time
/// plaintext secret.
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

struct ApiServerProc {
    child: Child,
    api_port: u16,
}

impl ApiServerProc {
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.api_port)
    }
}

/// Start the calod binary with the REST API enabled and wait for readiness.
async fn start_server(data_dir: &Path) -> ApiServerProc {
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
        // Keep the scheduled-snapshot loop out of these tests.
        .env("CALOD_PERSISTENCE__SNAPSHOT_INTERVAL_SECS", "3600")
        .env("CALOD_API__ENABLED", "true")
        .env("CALOD_API__LISTEN_ADDR", format!("127.0.0.1:{api_port}"))
        .env("CALOD_API__DATA_DIR", data_dir.join("ledger"))
        // `always`: an acknowledged write is fsynced, so the kill-restart
        // test asserts the durability contract exactly.
        .env("CALOD_API__DURABILITY", "always")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn calod server");

    let server = ApiServerProc { child, api_port };
    wait_ready(&server).await;
    server
}

async fn wait_ready(server: &ApiServerProc) {
    let client = Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(resp) = client
            .get(format!("{}/health/ready", server.base()))
            .send()
            .await
        {
            if resp.status() == StatusCode::OK {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server did not become ready in time"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Hard kill (no graceful shutdown, no flush): the crash-durability contract
/// under test.
async fn hard_kill(server: &mut ApiServerProc) {
    server.child.kill().await.expect("kill server");
    server.child.wait().await.expect("wait for killed server");
}

fn api_key(key: &str) -> String {
    key.to_string()
}

async fn get(client: &Client, url: &str, key: Option<&str>) -> Response {
    let mut req = client.get(url);
    if let Some(key) = key {
        req = req.header("x-calod-key", api_key(key));
    }
    req.send().await.expect("GET")
}

async fn post_json(
    client: &Client,
    url: &str,
    key: Option<&str>,
    idem: &str,
    body: Value,
) -> Response {
    let mut req = client.post(url).header("idempotency-key", idem).json(&body);
    if let Some(key) = key {
        req = req.header("x-calod-key", api_key(key));
    }
    req.send().await.expect("POST")
}

async fn patch_json(
    client: &Client,
    url: &str,
    key: &str,
    if_match: Option<&str>,
    body: Value,
) -> Response {
    let mut req = client
        .patch(url)
        .header("x-calod-key", api_key(key))
        .json(&body);
    if let Some(tag) = if_match {
        req = req.header("if-match", tag.to_string());
    }
    req.send().await.expect("PATCH")
}

async fn problem(resp: Response) -> Value {
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "errors must be RFC 7807 problem+json"
    );
    assert!(
        resp.headers().get("x-request-id").is_some(),
        "errors must carry X-Request-ID"
    );
    resp.json::<Value>().await.expect("problem body")
}

fn acquire_body(input: Value) -> Value {
    json!({
        "scope": "tenant-test:order-1",
        "tool_name": "stripe.charge",
        "input": input,
    })
}

// ---------------------------------------------------------------------------
// System endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_version_endpoints_shape() {
    let dir = temp_dir();
    let server = start_server(&dir).await;
    let client = Client::new();

    for path in ["/health/live", "/health/ready", "/version"] {
        let resp = get(&client, &format!("{}{}", server.base(), path), None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path} must be public");
        assert!(
            resp.headers().get("x-request-id").is_some(),
            "{path} must carry X-Request-ID"
        );
    }

    let live: Value = get(&client, &format!("{}/health/live", server.base()), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(live, json!({ "status": "ok" }));

    let version: Value = get(&client, &format!("{}/version", server.base()), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(version["api_version"], "v1");
    assert!(version["server_version"].is_string());
    assert_eq!(version["status"], "alpha");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_missing_or_invalid_key_is_401_problem() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");
    let key = create_api_key(&dir, &["effects:read", "effects:write"]);
    let server = start_server(&dir).await;
    let client = Client::new();
    let url = format!("{}/runs", server.base());

    // No key at all.
    let resp = get(&client, &url, None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = problem(resp).await;
    assert_eq!(body["code"], "INVALID_API_KEY");
    assert_eq!(body["status"], 401);
    assert_eq!(body["type"], "https://calod.dev/problems/invalid-api-key");
    assert!(body["request_id"].is_string());

    // Well-formed but unknown key.
    let bad = format!("cld_{}", "0".repeat(64));
    let resp = get(&client, &url, Some(&bad)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(problem(resp).await["code"], "INVALID_API_KEY");

    // Not even the right shape.
    let resp = get(&client, &url, Some("nope")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Valid key works.
    let resp = get(&client, &url, Some(&key)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Value = resp.json().await.unwrap();
    assert_eq!(list["data"], json!([]));
    assert_eq!(list["pagination"]["has_more"], false);

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reconcile_without_scope_is_403() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");
    // No `effects:reconcile` scope on this key.
    let key = create_api_key(&dir, &["effects:read", "effects:write"]);
    let server = start_server(&dir).await;
    let client = Client::new();

    let resp = post_json(
        &client,
        &format!("{}/effects", server.base()),
        Some(&key),
        "reconcile-scope-check",
        acquire_body(json!({ "a": 1 })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let decision: Value = resp.json().await.unwrap();
    let effect_id = decision["effect"]["id"].as_str().unwrap();
    let fencing = decision["effect"]["fencing_token"].as_i64().unwrap();

    // A reconcile transition without the scope is 403 before any state check.
    let resp = patch_json(
        &client,
        &format!("{}/effects/{}", server.base(), effect_id),
        &key,
        Some(&format!("\"{fencing}\"")),
        json!({ "transition": "reconcile_committed", "fencing_token": fencing }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = problem(resp).await;
    assert_eq!(body["code"], "INSUFFICIENT_SCOPE");
    assert_eq!(
        body["type"],
        "https://calod.dev/problems/insufficient-scope"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P2 exit suite
// ---------------------------------------------------------------------------

/// (a) 100 concurrent acquires on one idempotency key: exactly one 201
/// execute, 99 × 202 wait; the winning caller executes the tool once
/// (harness-counted) and commits; 100 retries then all replay the identical
/// receipt without executing again.
#[tokio::test]
async fn p2_concurrent_acquire_one_execute_then_replays() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");
    let key = create_api_key(
        &dir,
        &["effects:read", "effects:write", "effects:reconcile"],
    );
    let server = start_server(&dir).await;
    let client = Client::new();
    let url = format!("{}/effects", server.base());
    let executions = Arc::new(AtomicUsize::new(0));

    // 100 concurrent POSTs, same Idempotency-Key and same input.
    let mut tasks = Vec::new();
    for _ in 0..100 {
        let client = client.clone();
        let url = url.clone();
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            let resp = post_json(
                &client,
                &url,
                Some(&key),
                "concurrent-charge-0001",
                acquire_body(json!({ "amount_minor": 5000, "currency": "usd" })),
            )
            .await;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap();
            (status, body)
        }));
    }
    let mut winner: Option<Value> = None;
    let mut waits = 0usize;
    for task in tasks {
        let (status, body) = task.await.expect("acquire task");
        match status {
            StatusCode::CREATED => {
                assert_eq!(body["decision"], "execute");
                assert!(
                    body["lease_token"].is_string(),
                    "execute carries a lease token"
                );
                assert!(winner.replace(body).is_none(), "exactly one 201 execute");
            }
            StatusCode::ACCEPTED => {
                assert_eq!(body["decision"], "wait");
                waits += 1;
            }
            other => panic!("unexpected acquire status {other}: {body}"),
        }
    }
    let winner = winner.expect("one execute decision");
    assert_eq!(waits, 99, "the other 99 acquires must wait");

    // The single winner is the only caller that "runs the tool".
    executions.fetch_add(1, Ordering::SeqCst);
    let effect_id = winner["effect"]["id"].as_str().unwrap().to_string();
    let fencing = winner["effect"]["fencing_token"].as_i64().unwrap();
    let lease_token = winner["lease_token"].as_str().unwrap().to_string();
    let result = json!({ "charge_id": "ch_test_1" });
    let resp = patch_json(
        &client,
        &format!("{}/effects/{}", server.base(), effect_id),
        &key,
        Some(&format!("\"{fencing}\"")),
        json!({
            "transition": "commit",
            "lease_token": lease_token,
            "fencing_token": fencing,
            "result": result,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "commit must succeed");
    assert_eq!(resp.json::<Value>().await.unwrap()["state"], "committed");

    // 100 retries: all replay the identical receipt, no further execution.
    let mut tasks = Vec::new();
    for _ in 0..100 {
        let client = client.clone();
        let url = url.clone();
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            let resp = post_json(
                &client,
                &url,
                Some(&key),
                "concurrent-charge-0001",
                acquire_body(json!({ "amount_minor": 5000, "currency": "usd" })),
            )
            .await;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap();
            (status, body)
        }));
    }
    for task in tasks {
        let (status, body) = task.await.expect("retry task");
        assert_eq!(status, StatusCode::OK, "committed effect must replay");
        assert_eq!(body["decision"], "replay");
        assert_eq!(body["effect"]["id"], effect_id);
        assert_eq!(body["effect"]["state"], "committed");
        assert_eq!(body["effect"]["result"], result);
        assert_eq!(
            body["effect"]["input_digest"],
            winner["effect"]["input_digest"]
        );
        assert!(body["lease_token"].is_null(), "replay issues no lease");
    }
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the tool executed exactly once across 200 requests"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (b) A stale fencing token can never move the state machine: 409 LEASE_LOST.
#[tokio::test]
async fn p2_stale_fencing_token_conflict() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");
    let key = create_api_key(&dir, &["effects:write"]);
    let server = start_server(&dir).await;
    let client = Client::new();
    let url = format!("{}/effects", server.base());

    // Acquire (fencing 1), report a retryable failure, reacquire (fencing 2).
    let resp = post_json(
        &client,
        &url,
        Some(&key),
        "stale-fencing-00001",
        acquire_body(json!({ "n": 1 })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let first: Value = resp.json().await.unwrap();
    let effect_id = first["effect"]["id"].as_str().unwrap().to_string();
    let token1 = first["lease_token"].as_str().unwrap().to_string();
    let effect_url = format!("{}/effects/{}", server.base(), effect_id);

    let resp = patch_json(
        &client,
        &effect_url,
        &key,
        Some("\"1\""),
        json!({
            "transition": "fail_retryable",
            "lease_token": token1,
            "fencing_token": 1,
            "failure": { "kind": "connect_error", "message": "never sent", "retryable": true },
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = post_json(
        &client,
        &url,
        Some(&key),
        "stale-fencing-00001",
        acquire_body(json!({ "n": 1 })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "safe reacquire");
    let second: Value = resp.json().await.unwrap();
    assert_eq!(second["effect"]["fencing_token"], 2);
    let token2 = second["lease_token"].as_str().unwrap().to_string();

    // The stale fencing token (1) with the CURRENT ETag passes the If-Match
    // precondition but must still lose at the fencing check.
    let resp = patch_json(
        &client,
        &effect_url,
        &key,
        Some("\"2\""),
        json!({
            "transition": "commit",
            "lease_token": token2,
            "fencing_token": 1,
            "result": { "ok": true },
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = problem(resp).await;
    assert_eq!(body["code"], "LEASE_LOST");
    assert_eq!(body["type"], "https://calod.dev/problems/lease-lost");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (c) The same idempotency key with a different canonical input digest is a
/// 409 IDEMPOTENCY_CONFLICT.
#[tokio::test]
async fn p2_same_key_different_input_conflict() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");
    let key = create_api_key(&dir, &["effects:write"]);
    let server = start_server(&dir).await;
    let client = Client::new();
    let url = format!("{}/effects", server.base());

    let resp = post_json(
        &client,
        &url,
        Some(&key),
        "digest-conflict-001",
        acquire_body(json!({ "amount": 100 })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = post_json(
        &client,
        &url,
        Some(&key),
        "digest-conflict-001",
        acquire_body(json!({ "amount": 200 })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = problem(resp).await;
    assert_eq!(body["code"], "IDEMPOTENCY_CONFLICT");
    assert_eq!(
        body["type"],
        "https://calod.dev/problems/idempotency-conflict"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (d) Crash simulation: acquire + start, kill before commit, restart; after
/// the lease lapses the effect acquires as 200 reconcile and never hands out
/// another execute lease until reconciled.
#[tokio::test]
async fn p2_crash_before_commit_recovers_as_reconcile() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");
    let key = create_api_key(&dir, &["effects:write", "effects:reconcile"]);
    let mut server = start_server(&dir).await;
    let client = Client::new();
    let url = format!("{}/effects", server.base());

    // Acquire with the minimum lease (5s) and explicitly start the tool.
    let resp = post_json(
        &client,
        &url,
        Some(&key),
        "crash-recovery-0001",
        json!({
            "scope": "tenant-test:order-1",
            "tool_name": "stripe.charge",
            "input": { "amount": 500 },
            "lease_duration_seconds": 5,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let decision: Value = resp.json().await.unwrap();
    let effect_id = decision["effect"]["id"].as_str().unwrap().to_string();
    let lease_token = decision["lease_token"].as_str().unwrap().to_string();
    let effect_url = format!("{}/effects/{}", server.base(), effect_id);

    let resp = patch_json(
        &client,
        &effect_url,
        &key,
        Some("\"1\""),
        json!({ "transition": "start", "lease_token": lease_token, "fencing_token": 1 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "claimed -> in_progress");
    assert_eq!(resp.json::<Value>().await.unwrap()["state"], "in_progress");

    // Crash before commit: hard kill, no flush, then restart on the same dir.
    hard_kill(&mut server).await;
    let server = start_server(&dir).await;
    let url = format!("{}/effects", server.base());
    let effect_url = format!("{}/effects/{}", server.base(), effect_id);

    // Let the lease lapse: in_progress + expired lease => outcome_unknown.
    tokio::time::sleep(Duration::from_millis(5200)).await;

    let resp = post_json(
        &client,
        &url,
        Some(&key),
        "crash-recovery-0001",
        acquire_body(json!({ "amount": 500 })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["decision"], "reconcile",
        "unproven outcome must reconcile"
    );
    assert_eq!(body["effect"]["state"], "outcome_unknown");
    assert!(
        body["lease_token"].is_null(),
        "no lease for outcome_unknown"
    );

    // It never returns execute again until reconciled.
    let resp = post_json(
        &client,
        &url,
        Some(&key),
        "crash-recovery-0001",
        acquire_body(json!({ "amount": 500 })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["decision"], "reconcile");

    // Reconcile (the tool did fire): committed receipt, then replay forever.
    let resp = patch_json(
        &client,
        &effect_url,
        &key,
        Some("\"1\""),
        json!({
            "transition": "reconcile_committed",
            "fencing_token": 1,
            "result": { "charge_id": "ch_recovered" },
            "external_receipt": { "provider": "stripe", "id": "ch_recovered" },
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "reconcile with scope");
    assert_eq!(
        resp.json::<Value>().await.unwrap()["state"],
        "reconciled_committed"
    );

    let resp = post_json(
        &client,
        &url,
        Some(&key),
        "crash-recovery-0001",
        acquire_body(json!({ "amount": 500 })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["decision"], "replay");
    assert_eq!(
        body["effect"]["result"],
        json!({ "charge_id": "ch_recovered" })
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (e) ETag/If-Match happy path on runs, then 412 on a stale precondition.
#[tokio::test]
async fn run_etag_if_match_happy_path_and_412() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");
    let key = create_api_key(&dir, &["effects:write"]);
    let server = start_server(&dir).await;
    let client = Client::new();

    let resp = post_json(
        &client,
        &format!("{}/runs", server.base()),
        Some(&key),
        "run-etag-test-0001",
        json!({ "external_id": "run-1", "agent_name": "order-agent" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get("etag").and_then(|v| v.to_str().ok()),
        Some("\"1\""),
        "new runs carry ETag of version 1"
    );
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("Location header")
        .to_string();
    let run: Value = resp.json().await.unwrap();
    assert_eq!(run["version"], 1);

    let run_url = format!("{}{}", server.base().trim_end_matches("/v1"), location);
    let resp = get(&client, &run_url, Some(&key)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("etag").and_then(|v| v.to_str().ok()),
        Some("\"1\"")
    );

    // Happy path: If-Match "1" transitions the run.
    let resp = patch_json(
        &client,
        &run_url,
        &key,
        Some("\"1\""),
        json!({ "status": "suspended" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("etag").and_then(|v| v.to_str().ok()),
        Some("\"2\""),
        "transition bumps the ETag"
    );
    assert_eq!(resp.json::<Value>().await.unwrap()["status"], "suspended");

    // Stale precondition: 412 problem.
    let resp = patch_json(
        &client,
        &run_url,
        &key,
        Some("\"1\""),
        json!({ "status": "running" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let body = problem(resp).await;
    assert_eq!(body["code"], "PRECONDITION_FAILED");
    assert_eq!(
        body["type"],
        "https://calod.dev/problems/precondition-failed"
    );

    // Missing If-Match is a 400 validation problem (header is required).
    let resp = patch_json(
        &client,
        &run_url,
        &key,
        None,
        json!({ "status": "running" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem(resp).await["code"], "VALIDATION_ERROR");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cursor pagination envelope: `data` + `pagination{next_cursor, has_more,
/// limit}`, cursors are opaque and paginate stably.
#[tokio::test]
async fn list_runs_cursor_pagination() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir");
    let key = create_api_key(&dir, &["effects:read"]);
    let server = start_server(&dir).await;
    let client = Client::new();
    let url = format!("{}/runs", server.base());

    for i in 0..3 {
        let resp = post_json(
            &client,
            &url,
            Some(&key),
            &format!("run-page-create-{i:04}"),
            json!({ "external_id": format!("run-{i}"), "agent_name": "pager" }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let resp = get(&client, &format!("{url}?limit=2"), Some(&key)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let page1: Value = resp.json().await.unwrap();
    assert_eq!(page1["data"].as_array().unwrap().len(), 2);
    assert_eq!(page1["pagination"]["has_more"], true);
    assert_eq!(page1["pagination"]["limit"], 2);
    let cursor = page1["pagination"]["next_cursor"]
        .as_str()
        .expect("cursor for page 2");

    let resp = get(
        &client,
        &format!("{url}?limit=2&cursor={cursor}"),
        Some(&key),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let page2: Value = resp.json().await.unwrap();
    assert_eq!(page2["data"].as_array().unwrap().len(), 1);
    assert_eq!(page2["pagination"]["has_more"], false);
    assert!(page2["pagination"]["next_cursor"].is_null());

    // Pages are disjoint and cover everything.
    let mut ids: Vec<String> = page1["data"]
        .as_array()
        .unwrap()
        .iter()
        .chain(page2["data"].as_array().unwrap())
        .map(|run| run["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3);

    // A malformed cursor is a 400 validation problem.
    let resp = get(&client, &format!("{url}?cursor=garbage"), Some(&key)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem(resp).await["code"], "VALIDATION_ERROR");

    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
